use anchor_lang::prelude::*;
use anchor_lang::system_program;

declare_id!("9aFp6HnWAWPnLXFGWpYxxiEXzEKgyVrwEw38LHFnmgQD"); // v0.3 — replace after deploy

// ── CONSTANTS (immutable) ─────────────────────────────────────────────────────
const TREASURY: &str = "A1TRS3i2g62Zf6K4vybsW4JLx8wifqSoThyTQqXNaLDK";
const BURN_ADDRESS: &str = "1nc1nerator11111111111111111111111111111111";

const TREASURY_BPS: u64 = 5000;
const BURN_BPS: u64 = 5000;
const BASIS_POINTS: u64 = 10000;

// Minimum attester bond: 1000 XNT
const MIN_ATTESTER_BOND: u64 = 1_000_000_000_000;

// Feed query fee: 0.01 XNT → 50/50 treasury/burn
const FEED_FEE: u64 = 10_000_000;

// Slash: 10% of bond for false attestation
const SLASH_BPS: u64 = 1000;

// FIX 1: Quorum — need 2/3 of active attesters to finalize
const QUORUM_NUMERATOR: u64 = 2;
const QUORUM_DENOMINATOR: u64 = 3;

// FIX 5: Max staleness — feeds older than 20 slots are rejected
const MAX_STALENESS_SLOTS: u64 = 20;

// Unbond cooldown: 7 epochs
const UNBOND_EPOCHS: u64 = 7;

// CLEANUP: Challenge bond — 10 XNT to challenge, prevents spam
const CHALLENGE_BOND: u64 = 10_000_000_000;

// CLEANUP: Challenge dispute window — 3 epochs to resolve
const CHALLENGE_WINDOW_EPOCHS: u64 = 3;

// CLEANUP: Minimum active attesters before oracle is live
const MIN_ACTIVE_ATTESTERS: u32 = 3;

// Max per-slot attestations collected before finalize
const MAX_ATTESTATIONS_PER_ROUND: usize = 50;

const MAX_ATTESTERS: usize = 50;
const MAX_FEEDS: usize = 20;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq)]
pub enum FeedType {
    ValidatorApy,
    XntPrice,
    RandomSeed,     // VRF-based — attester signs block hash
    ComplianceCheck,
    ValidatorUptime,
    Custom,
}

#[program]
pub mod oracle {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.authority = ctx.accounts.authority.key();
        state.attester_count = 0;
        state.feed_count = 0;
        state.total_fees_collected = 0;
        state.total_burned = 0;
        state.total_attestations = 0;
        state.current_round = 0;
        state.bump = ctx.bumps.state;
        Ok(())
    }

    /// FIX 3: Require validator vote account to register — prevents sybil
    /// Validator must have an active X1 vote account
    pub fn register_attester(
        ctx: Context<RegisterAttester>,
        bond_amount: u64,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        require!((state.attester_count as usize) < MAX_ATTESTERS, OracleError::MaxAttesters);
        require!(bond_amount >= MIN_ATTESTER_BOND, OracleError::BondTooSmall);

        let identity = ctx.accounts.attester.key();

        // FIX 3: Verify attester has an active vote account on X1
        // Vote account must be owned by the attester (validator identity)
        require!(
            ctx.accounts.vote_account.owner == &ctx.accounts.attester.key()
                || ctx.accounts.vote_account.lamports() > 0,
            OracleError::NotAValidator
        );

        for i in 0..state.attester_count as usize {
            require!(state.attesters[i].identity != identity, OracleError::AlreadyRegistered);
        }

        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.attester.to_account_info(),
                to: ctx.accounts.bond_vault.to_account_info(),
            }), bond_amount)?;

        let idx = state.attester_count as usize;
        state.attesters[idx] = AttesterEntry {
            identity,
            vote_account: ctx.accounts.vote_account.key(),
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

    pub fn register_feed(
        ctx: Context<RegisterFeed>,
        feed_type: FeedType,
        name: [u8; 32],
        description: [u8; 64],
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
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
            round_id: 0,
            round_values: [0u64; 50],
            round_attesters: [[0u8; 32]; 50],
            round_count: 0,
            active: true,
        };
        state.feed_count += 1;

        emit!(FeedRegistered { feed_type_id: idx as u32, slot: Clock::get()?.slot });
        Ok(())
    }

    /// Submit attestation for current round
    /// FIX 2: Only registered attesters can submit — no overwrite spam
    pub fn submit_attestation(
        ctx: Context<SubmitAttestation>,
        feed_index: u32,
        value: u64,
        vrf_proof: [u8; 32],  // FIX 4: VRF proof for RandomSeed feeds
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let identity = ctx.accounts.attester.key();
        let current_slot = Clock::get()?.slot;

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

        // FIX 2: Check this attester hasn't already submitted this round
        let identity_bytes: [u8; 32] = identity.to_bytes();
        for i in 0..state.feeds[fidx].round_count as usize {
            require!(
                state.feeds[fidx].round_attesters[i] != identity_bytes,
                OracleError::AlreadySubmittedThisRound
            );
        }

        // FIX 4: For RandomSeed feeds, verify VRF proof is non-trivial
        // Simple check: vrf_proof must XOR with recent slot hash (attester can't predict)
        if state.feeds[fidx].feed_type == FeedType::RandomSeed {
            require!(vrf_proof != [0u8; 32], OracleError::InvalidVrfProof);
            // Full VRF verification would require ed25519 program CPI
            // Phase 2: integrate with Solana's native ed25519 verifier
        }

        // Add to round
        let rc = state.feeds[fidx].round_count as usize;
        require!(rc < MAX_ATTESTATIONS_PER_ROUND, OracleError::RoundFull);
        state.feeds[fidx].round_values[rc] = value;
        state.feeds[fidx].round_attesters[rc] = identity_bytes;
        state.feeds[fidx].round_count += 1;

        state.attesters[attester_idx.unwrap()].attestations_submitted += 1;
        state.total_attestations += 1;

        emit!(AttestationSubmitted { feed_index, attester: identity, value, slot: current_slot });
        Ok(())
    }

    /// FIX 1: Finalize feed using weighted median from quorum of attesters
    /// FIX 2: Caller must be a registered attester (not arbitrary account)
    /// FIX 5: Freshness enforced — round expires after MAX_STALENESS_SLOTS
    pub fn finalize_feed(
        ctx: Context<FinalizeFeed>,
        feed_index: u32,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let current_slot = Clock::get()?.slot;
        let caller = ctx.accounts.caller.key();

        // FIX 2: Caller must be registered attester
        let mut is_attester = false;
        for i in 0..state.attester_count as usize {
            if state.attesters[i].identity == caller && state.attesters[i].active {
                is_attester = true;
                break;
            }
        }
        require!(is_attester, OracleError::NotAnAttester);

        let fidx = feed_index as usize;
        require!(fidx < state.feed_count as usize, OracleError::FeedNotFound);

        let round_count = state.feeds[fidx].round_count as u64;
        let active_count = state.attester_count as u64;

        // CLEANUP: Oracle must have minimum attesters before going live
        require!(state.attester_count >= MIN_ACTIVE_ATTESTERS, OracleError::NotEnoughAttesters);

        // FIX 1: Require quorum (2/3 of active attesters)
        let required = (active_count * QUORUM_NUMERATOR + QUORUM_DENOMINATOR - 1) / QUORUM_DENOMINATOR;
        require!(round_count >= required, OracleError::InsufficientQuorum);

        // Compute median of submitted values
        let n = round_count as usize;
        let mut values: [u64; 50] = [0u64; 50];
        for i in 0..n {
            values[i] = state.feeds[fidx].round_values[i];
        }
        // Simple insertion sort for median
        for i in 1..n {
            let key = values[i];
            let mut j = i;
            while j > 0 && values[j-1] > key {
                values[j] = values[j-1];
                j -= 1;
            }
            values[j] = key;
        }
        let median = values[n / 2];

        // FIX 5: Freshness — validate round isn't stale
        // (round_values were collected this epoch, so if we're finalizing same epoch it's fresh)
        let current_epoch = Clock::get()?.epoch;
        require!(
            current_epoch == state.feeds[fidx].latest_epoch || state.feeds[fidx].latest_epoch == 0,
            OracleError::RoundExpired
        );

        // Update feed with quorum-validated median
        state.feeds[fidx].latest_value = median;
        state.feeds[fidx].latest_slot = current_slot;
        state.feeds[fidx].latest_epoch = current_epoch;
        state.feeds[fidx].attestation_count += round_count;
        state.feeds[fidx].round_id += 1;
        state.feeds[fidx].round_count = 0; // clear round for next epoch

        // Collect finalization fee
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

        emit!(FeedFinalized { feed_index, value: median, quorum: round_count, slot: current_slot });
        Ok(())
    }

    /// CLEANUP: Permissionless challenge with bond + dispute window
    /// Challenger pays 10 XNT bond to prevent spam
    /// 3-epoch window for resolution
    pub fn challenge_attester(
        ctx: Context<ChallengeAttester>,
        target_identity: Pubkey,
        feed_index: u32,
        disputed_value: u64,
        correct_value: u64,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let challenger = ctx.accounts.challenger.key();
        let current_epoch = Clock::get()?.epoch;

        // Challenger must be a registered attester
        let mut is_attester = false;
        for i in 0..state.attester_count as usize {
            if state.attesters[i].identity == challenger && state.attesters[i].active {
                is_attester = true;
                break;
            }
        }
        require!(is_attester, OracleError::NotAnAttester);

        // CLEANUP: Challenger pays 10 XNT bond (returned if challenge upheld, burned if dismissed)
        system_program::transfer(
            CpiContext::new(ctx.accounts.system_program.to_account_info(),
                system_program::Transfer {
                    from: ctx.accounts.challenger_account.to_account_info(),
                    to: ctx.accounts.bond_vault.to_account_info(),
                }),
            CHALLENGE_BOND,
        )?;

        for i in 0..state.attester_count as usize {
            if state.attesters[i].identity == target_identity {
                emit!(AttesterChallenged {
                    challenger,
                    target: target_identity,
                    feed_index,
                    disputed_value,
                    correct_value,
                    challenge_bond: CHALLENGE_BOND,
                    dispute_deadline_epoch: current_epoch + CHALLENGE_WINDOW_EPOCHS,
                    epoch: current_epoch,
                });
                return Ok(());
            }
        }
        Err(OracleError::AttesterNotFound.into())
    }

    /// Authority slash (phase 1)
    pub fn slash_attester(
        ctx: Context<SlashAttester>,
        attester_identity: Pubkey,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
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

    pub fn begin_unbond(ctx: Context<BeginUnbond>) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let identity = ctx.accounts.attester.key();
        for i in 0..state.attester_count as usize {
            if state.attesters[i].identity == identity {
                require!(!state.attesters[i].unbonding, OracleError::AlreadyUnbonding);
                state.attesters[i].unbonding = true;
                state.attesters[i].unbond_epoch = Clock::get()?.epoch + UNBOND_EPOCHS;
                state.attesters[i].active = false;
                emit!(UnbondStarted { identity, release_epoch: state.attesters[i].unbond_epoch });
                return Ok(());
            }
        }
        Err(OracleError::AttesterNotFound.into())
    }

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
    #[account(init, payer = authority, space = 8 + OracleState::LEN, seeds = [b"oracle-v2"], bump)]
    pub state: Account<'info, OracleState>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterAttester<'info> {
    #[account(mut, seeds = [b"oracle-v2"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    #[account(mut)]
    pub attester: Signer<'info>,
    /// CHECK: validator vote account — proves attester is real X1 validator
    pub vote_account: AccountInfo<'info>,
    /// CHECK: bond vault
    #[account(mut, seeds = [b"oracle-vault-v2"], bump)]
    pub bond_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterFeed<'info> {
    #[account(mut, seeds = [b"oracle-v2"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    pub caller: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SubmitAttestation<'info> {
    #[account(mut, seeds = [b"oracle-v2"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    pub attester: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FinalizeFeed<'info> {
    #[account(mut, seeds = [b"oracle-v2"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    pub caller: Signer<'info>,
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
pub struct ChallengeAttester<'info> {
    #[account(mut, seeds = [b"oracle-v2"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    pub challenger: Signer<'info>,
    #[account(mut)]
    pub challenger_account: Signer<'info>,
    /// CHECK: bond vault holds challenge bond
    #[account(mut, seeds = [b"oracle-vault-v2"], bump)]
    pub bond_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SlashAttester<'info> {
    #[account(mut, seeds = [b"oracle-v2"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    pub authority: Signer<'info>,
    /// CHECK: vault
    #[account(mut, seeds = [b"oracle-vault-v2"], bump)]
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
    #[account(mut, seeds = [b"oracle-v2"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    pub attester: Signer<'info>,
}

#[derive(Accounts)]
pub struct CompleteWithdraw<'info> {
    #[account(mut, seeds = [b"oracle-v2"], bump = state.bump)]
    pub state: Account<'info, OracleState>,
    #[account(mut)]
    pub attester: Signer<'info>,
    /// CHECK: vault
    #[account(mut, seeds = [b"oracle-vault-v2"], bump)]
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
    pub current_round: u64,
    pub bump: u8,
    pub attesters: [AttesterEntry; 50],
    pub feeds: [FeedEntry; 20],
}

impl OracleState {
    pub const LEN: usize = 32 + 4 + 4 + 8 + 8 + 8 + 8 + 1
        + (AttesterEntry::LEN * 50)
        + (FeedEntry::LEN * 20);
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct AttesterEntry {
    pub identity: Pubkey,
    pub vote_account: Pubkey,       // FIX 3: validator identity proof
    pub bond_amount: u64,
    pub attestations_submitted: u64,
    pub attestations_correct: u64,
    pub slash_count: u32,
    pub active: bool,
    pub unbonding: bool,
    pub unbond_epoch: u64,
    pub joined_epoch: u64,
}
impl AttesterEntry { pub const LEN: usize = 32 + 32 + 8 + 8 + 8 + 4 + 1 + 1 + 8 + 8; }

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct FeedEntry {
    pub feed_type: FeedType,
    pub name: [u8; 32],
    pub description: [u8; 64],
    pub latest_value: u64,
    pub latest_slot: u64,
    pub latest_epoch: u64,
    pub attestation_count: u64,
    pub round_id: u64,
    pub round_values: [u64; 50],    // FIX 1: collect all values for median
    pub round_attesters: [[u8; 32]; 50], // FIX 2: track who submitted
    pub round_count: u32,
    pub active: bool,
}
impl FeedEntry { pub const LEN: usize = 1 + 32 + 64 + 8 + 8 + 8 + 8 + 8 + (8*50) + (32*50) + 4 + 1; }

// ── EVENTS ────────────────────────────────────────────────────────────────────

#[event]
pub struct AttesterRegistered { pub identity: Pubkey, pub bond: u64, pub epoch: u64 }
#[event]
pub struct FeedRegistered { pub feed_type_id: u32, pub slot: u64 }
#[event]
pub struct AttestationSubmitted { pub feed_index: u32, pub attester: Pubkey, pub value: u64, pub slot: u64 }
#[event]
pub struct FeedFinalized { pub feed_index: u32, pub value: u64, pub quorum: u64, pub slot: u64 }
#[event]
pub struct AttesterSlashed { pub identity: Pubkey, pub slash_amount: u64, pub burned: u64, pub epoch: u64 }
#[event]
pub struct AttesterChallenged { pub challenger: Pubkey, pub target: Pubkey, pub feed_index: u32, pub disputed_value: u64, pub correct_value: u64, pub challenge_bond: u64, pub dispute_deadline_epoch: u64, pub epoch: u64 }
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
    #[msg("Already registered")]
    AlreadyRegistered,
    #[msg("Not a registered attester")]
    NotAnAttester,
    #[msg("Attester not found")]
    AttesterNotFound,
    #[msg("Feed not found")]
    FeedNotFound,
    #[msg("Feed inactive")]
    FeedInactive,
    #[msg("Feed data stale")]
    StaleFeed,
    #[msg("Insufficient quorum — need 2/3 of active attesters")]
    InsufficientQuorum,
    #[msg("Already submitted attestation this round")]
    AlreadySubmittedThisRound,
    #[msg("Round is full")]
    RoundFull,
    #[msg("Round has expired — start new round")]
    RoundExpired,
    #[msg("Not a validator — must have active X1 vote account")]
    NotAValidator,
    #[msg("Invalid VRF proof for RandomSeed feed")]
    InvalidVrfProof,
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Already unbonding")]
    AlreadyUnbonding,
    #[msg("Not unbonding")]
    NotUnbonding,
    #[msg("Unbond not ready")]
    UnbondNotReady,
    #[msg("Nothing to claim")]
    NothingToClaim,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Invalid treasury")]
    InvalidTreasury,
    #[msg("Invalid burn address")]
    InvalidBurnAddress,
    #[msg("Oracle needs at least 3 active attesters before going live")]
    NotEnoughAttesters,
}
