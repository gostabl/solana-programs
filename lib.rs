use anchor_lang::prelude::*;
use anchor_lang::AccountDeserialize;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};
use std::collections::BTreeSet;

declare_id!("C2MWoJDtVx4LqyPywv6yYdvTKfHg3ZKs616ZYwxg2xxz");

#[program]
pub mod solana_payment_splitter {
    use super::*;

    /// v2: percentages only; recipient token accounts passed via remaining_accounts.
    pub fn split_payment<'info>(
        ctx: Context<'_, '_, 'info, 'info, SplitPayment<'info>>,
        percentages_bps: Vec<u16>,
        platform_fee_basis_points: u16,
        max_recipients: u16,
        min_transfer_amount: u64,
        total_amount: u64,
    ) -> Result<()> {
        const MAX_BPS: u16 = 10_000;

        // --- Sanity checks
        let n = percentages_bps.len();
        require!(
            n > 0 && (n as u16) <= max_recipients,
            ErrorCode::InvalidRecipientCount
        );
        require!(total_amount > 0, ErrorCode::TotalAmountTooSmall);
        require!(
            ctx.accounts.token_program.key() == anchor_spl::token::ID,
            ErrorCode::InvalidTokenProgram
        );

        // Source TA checks (no extra allocs here)
        let src = &ctx.accounts.sender_token_account;
        require!(
            src.owner == ctx.accounts.sender.key(),
            ErrorCode::InvalidTokenAccountOwner
        );
        require!(src.mint == ctx.accounts.mint.key(), ErrorCode::MintMismatch);
        require!(src.amount >= total_amount, ErrorCode::InsufficientBalance);

        // Percentages must sum to 100%
        let mut sum: u16 = 0;
        for bps in &percentages_bps {
            require!(*bps > 0, ErrorCode::ZeroPercentage);
            sum = sum.checked_add(*bps).ok_or(ErrorCode::MathOverflow)?;
        }
        require!(sum == MAX_BPS, ErrorCode::InvalidPercentageSum);

        // Fee math (only platform fee now)
        require!(platform_fee_basis_points <= 1_000, ErrorCode::ExcessiveFee); // <= 10%

        let total_amount_u128 = total_amount as u128;
        let fee_u128 = total_amount_u128
            .checked_mul(platform_fee_basis_points as u128)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_div(MAX_BPS as u128)
            .ok_or(ErrorCode::MathOverflow)?;
        let fee_amount: u64 = fee_u128.try_into().map_err(|_| ErrorCode::MathOverflow)?;
        let distributable = total_amount
            .checked_sub(fee_amount)
            .ok_or(ErrorCode::MathOverflow)?;

        // remaining_accounts = [ recipients..., (fee_ta if fee>0) ]
        let expected = n + if fee_amount > 0 { 1 } else { 0 };
        require!(
            ctx.remaining_accounts.len() == expected,
            ErrorCode::IncorrectAccountCount
        );

        let decimals = ctx.accounts.mint.decimals;
        let token_program_ai = ctx.accounts.token_program.to_account_info();

        // --- Recipients loop (heap-lean)
        let mut total_distributed: u64 = 0;
        let dist_u128 = distributable as u128;

        for (i, bps) in percentages_bps.iter().enumerate() {
            let ai: &AccountInfo<'info> = &ctx.remaining_accounts[i];
            require!(ai.is_writable, ErrorCode::RecipientAccountNotWritable);

            // Lightweight parse of TokenAccount without Anchor's account loader allocations
            let ta = read_token_account(ai)?;
            require!(ta.mint == ctx.accounts.mint.key(), ErrorCode::MintMismatch);

            // Compute amount (last gets remainder)
            let amount: u64 = if i == n - 1 {
                distributable
                    .checked_sub(total_distributed)
                    .ok_or(ErrorCode::MathOverflow)?
            } else {
                let a_u128 = dist_u128
                    .checked_mul(*bps as u128)
                    .ok_or(ErrorCode::MathOverflow)?
                    .checked_div(MAX_BPS as u128)
                    .ok_or(ErrorCode::MathOverflow)?;
                let a: u64 = a_u128.try_into().map_err(|_| ErrorCode::MathOverflow)?;
                total_distributed = total_distributed
                    .checked_add(a)
                    .ok_or(ErrorCode::MathOverflow)?;
                a
            };

            require!(
                amount >= min_transfer_amount,
                ErrorCode::RecipientAmountTooSmall
            );

            // CPI: transfer_checked
            let cpi = TransferChecked {
                from: ctx.accounts.sender_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ai.clone(),
                authority: ctx.accounts.sender.to_account_info(),
            };
            let cpi_ctx = CpiContext::new(token_program_ai.clone(), cpi);
            token::transfer_checked(cpi_ctx, amount, decimals)?;
        }

        // --- Platform fee transfer (if any)
        if fee_amount > 0 {
            let fee_ai: &AccountInfo<'info> = &ctx.remaining_accounts[n];
            require!(fee_ai.is_writable, ErrorCode::FeeWalletNotWritable);

            let fee_ta = read_token_account(fee_ai)?;
            require!(
                fee_ta.mint == ctx.accounts.mint.key(),
                ErrorCode::MintMismatch
            );

            let cpi = TransferChecked {
                from: ctx.accounts.sender_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: fee_ai.clone(),
                authority: ctx.accounts.sender.to_account_info(),
            };
            let cpi_ctx = CpiContext::new(token_program_ai, cpi);
            token::transfer_checked(cpi_ctx, fee_amount, decimals)?;

            // single small event is fine
            emit!(PlatformFeeCollected {
                sender: ctx.accounts.sender.key(),
                amount: fee_amount,
                platform_fee_basis_points,
            });
        }

        Ok(())
    }

    /// Bulk payouts: exact per-recipient amounts; fee handled off-program.
    pub fn bulk_payout<'info>(
        ctx: Context<'_, '_, 'info, 'info, SplitPayment<'info>>,
        amounts: Vec<u64>,        // one amount per recipient ATA
        max_recipients: u16,      // policy cap
        min_transfer_amount: u64, // floor per recipient (base units)
        total_amount: u64,        // must equal sum(amounts)
    ) -> Result<()> {
        // Sanity
        let n = amounts.len();
        require!(
            n > 0 && (n as u16) <= max_recipients,
            ErrorCode::InvalidRecipientCount
        );
        require!(total_amount > 0, ErrorCode::TotalAmountTooSmall);
        require!(
            ctx.accounts.token_program.key() == anchor_spl::token::ID,
            ErrorCode::InvalidTokenProgram
        );

        // Source checks
        let src = &ctx.accounts.sender_token_account;
        require!(
            src.owner == ctx.accounts.sender.key(),
            ErrorCode::InvalidTokenAccountOwner
        );
        require!(src.mint == ctx.accounts.mint.key(), ErrorCode::MintMismatch);
        require!(src.amount >= total_amount, ErrorCode::InsufficientBalance);

        // remaining_accounts = [ recipients... ] (no fee account in bulk mode)
        require!(
            ctx.remaining_accounts.len() == n,
            ErrorCode::IncorrectAccountCount
        );

        // Verify sum(amounts)
        let mut sum: u128 = 0;
        for a in &amounts {
            require!(
                *a >= min_transfer_amount,
                ErrorCode::RecipientAmountTooSmall
            );
            sum = sum.checked_add(*a as u128).ok_or(ErrorCode::MathOverflow)?;
        }
        require!(sum as u64 == total_amount, ErrorCode::MathOverflow);

        // Dedup and mint checks
        {
            let mut seen = BTreeSet::new();
            for ai in ctx.remaining_accounts.iter() {
                require!(ai.is_writable, ErrorCode::RecipientAccountNotWritable);
                require!(seen.insert(ai.key()), ErrorCode::DuplicateRecipientAccount);
            }
        }

        let decimals = ctx.accounts.mint.decimals;
        let token_program_ai = ctx.accounts.token_program.to_account_info();

        // Stream the transfers, no per-recipient events (keeps heap tiny)
        for i in 0..n {
            // Parse and verify mint
            let ai: &AccountInfo<'info> = &ctx.remaining_accounts[i];
            let ta: Account<'info, TokenAccount> = Account::try_from(ai)?;
            require!(ta.mint == ctx.accounts.mint.key(), ErrorCode::MintMismatch);

            let amount = amounts[i];

            let cpi = TransferChecked {
                from: ctx.accounts.sender_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ai.clone(),
                authority: ctx.accounts.sender.to_account_info(),
            };
            let cpi_ctx = CpiContext::new(token_program_ai.clone(), cpi);
            token::transfer_checked(cpi_ctx, amount, decimals)?;
        }

        // One compact summary event
        emit!(BulkSummary {
            sender: ctx.accounts.sender.key(),
            total_recipients: n as u16,
            total_amount,
        });

        Ok(())
    }
}

// --- Minimal, allocation-lean helper to read TokenAccount ----------
fn read_token_account<'info>(ai: &AccountInfo<'info>) -> Result<TokenAccount> {
    // Borrow data, then deserialize without loader bookkeeping
    let data_ref = ai.try_borrow_data()?;
    let mut slice: &[u8] = &data_ref;
    TokenAccount::try_deserialize_unchecked(&mut slice)
        .map_err(|_| error!(ErrorCode::InvalidTokenAccountOwner))
}

// ----------------- Accounts & Events -----------------

#[derive(Accounts)]
pub struct SplitPayment<'info> {
    #[account(mut)]
    pub sender: Signer<'info>,

    #[account(mut)]
    pub sender_token_account: Account<'info, TokenAccount>,

    pub mint: Account<'info, Mint>,

    pub token_program: Program<'info, Token>,

    pub system_program: Program<'info, System>,
}

// Per-recipient event removed to reduce heap.
// Keep this small summary event.
#[event]
pub struct PlatformFeeCollected {
    pub sender: Pubkey,
    pub amount: u64,
    pub platform_fee_basis_points: u16,
}

#[event]
pub struct BulkSummary {
    pub sender: Pubkey,
    pub total_recipients: u16,
    pub total_amount: u64,
}

// ----------------- Errors -----------------

#[error_code]
pub enum ErrorCode {
    #[msg("Invalid token program")]
    InvalidTokenProgram,
    #[msg("Invalid number of recipients")]
    InvalidRecipientCount,
    #[msg("Percentages must sum to exactly 10000 bps")]
    InvalidPercentageSum,
    #[msg("Percentage cannot be zero")]
    ZeroPercentage,
    #[msg("Insufficient balance for total_amount")]
    InsufficientBalance,
    #[msg("Per-recipient transfer amount too small")]
    RecipientAmountTooSmall,
    #[msg("Math overflow detected")]
    MathOverflow,
    #[msg("Total fee exceeds maximum allowed cap")]
    ExcessiveFee,
    #[msg("Invalid token account owner for source")]
    InvalidTokenAccountOwner,
    #[msg("Recipient account must be writable")]
    RecipientAccountNotWritable,
    #[msg("Fee wallet must be writable")]
    FeeWalletNotWritable,
    #[msg("Incorrect number of remaining accounts")]
    IncorrectAccountCount,
    #[msg("Mint mismatch across accounts")]
    MintMismatch,
    #[msg("Duplicate recipient token account detected")]
    DuplicateRecipientAccount,
    #[msg("Total amount must be > 0")]
    TotalAmountTooSmall,
}
