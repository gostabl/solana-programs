use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};
use std::collections::BTreeSet;

declare_id!("6GpoKXf12t4egf42cndn7qjurBLtNkP72fTpYSjuidLE");

#[program]
pub mod solana_payment_splitter {
    use super::*;

    /// Bulk payouts: exact per-recipient token amounts; fees are handled off-program.
    pub fn bulk_payout<'info>(
        ctx: Context<'_, '_, 'info, 'info, BulkPayout<'info>>,
        amounts: Vec<u64>,
        max_recipients: u16,
        min_transfer_amount: u64,
        total_amount: u64,
    ) -> Result<()> {
        let n = amounts.len();
        require!(
            n > 0 && n <= max_recipients as usize,
            ErrorCode::InvalidRecipientCount
        );
        require!(total_amount > 0, ErrorCode::TotalAmountTooSmall);
        require!(
            ctx.accounts.token_program.key() == anchor_spl::token::ID,
            ErrorCode::InvalidTokenProgram
        );

        let src = &ctx.accounts.sender_token_account;
        require!(
            src.owner == ctx.accounts.sender.key(),
            ErrorCode::InvalidTokenAccountOwner
        );
        require!(src.mint == ctx.accounts.mint.key(), ErrorCode::MintMismatch);
        require!(src.amount >= total_amount, ErrorCode::InsufficientBalance);

        require!(
            ctx.remaining_accounts.len() == n,
            ErrorCode::IncorrectAccountCount
        );

        let mut sum: u128 = 0;
        for amount in &amounts {
            require!(
                *amount >= min_transfer_amount,
                ErrorCode::RecipientAmountTooSmall
            );
            sum = sum
                .checked_add(*amount as u128)
                .ok_or(ErrorCode::MathOverflow)?;
        }
        require!(
            sum == total_amount as u128,
            ErrorCode::TotalAmountMismatch
        );

        let mut seen = BTreeSet::new();
        for account_info in ctx.remaining_accounts.iter() {
            require!(
                account_info.is_writable,
                ErrorCode::RecipientAccountNotWritable
            );
            require!(
                seen.insert(account_info.key()),
                ErrorCode::DuplicateRecipientAccount
            );
        }

        let decimals = ctx.accounts.mint.decimals;
        let token_program = ctx.accounts.token_program.to_account_info();

        for (index, account_info) in ctx.remaining_accounts.iter().enumerate() {
            let recipient_token_account: Account<'info, TokenAccount> =
                Account::try_from(account_info)?;
            require!(
                recipient_token_account.mint == ctx.accounts.mint.key(),
                ErrorCode::MintMismatch
            );

            let cpi_accounts = TransferChecked {
                from: ctx.accounts.sender_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: account_info.clone(),
                authority: ctx.accounts.sender.to_account_info(),
            };
            let cpi_ctx = CpiContext::new(token_program.clone(), cpi_accounts);
            token::transfer_checked(cpi_ctx, amounts[index], decimals)?;
        }

        emit!(BulkSummary {
            sender: ctx.accounts.sender.key(),
            total_recipients: n as u16,
            total_amount,
        });

        Ok(())
    }
}

#[derive(Accounts)]
pub struct BulkPayout<'info> {
    #[account(mut)]
    pub sender: Signer<'info>,

    #[account(mut)]
    pub sender_token_account: Account<'info, TokenAccount>,

    pub mint: Account<'info, Mint>,

    pub token_program: Program<'info, Token>,

    pub system_program: Program<'info, System>,
}

#[event]
pub struct BulkSummary {
    pub sender: Pubkey,
    pub total_recipients: u16,
    pub total_amount: u64,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Invalid token program")]
    InvalidTokenProgram,
    #[msg("Invalid number of recipients")]
    InvalidRecipientCount,
    #[msg("Insufficient balance for total_amount")]
    InsufficientBalance,
    #[msg("Per-recipient transfer amount too small")]
    RecipientAmountTooSmall,
    #[msg("Math overflow detected")]
    MathOverflow,
    #[msg("Invalid token account owner for source")]
    InvalidTokenAccountOwner,
    #[msg("Recipient account must be writable")]
    RecipientAccountNotWritable,
    #[msg("Incorrect number of remaining accounts")]
    IncorrectAccountCount,
    #[msg("Mint mismatch across accounts")]
    MintMismatch,
    #[msg("Duplicate recipient token account detected")]
    DuplicateRecipientAccount,
    #[msg("Total amount must be > 0")]
    TotalAmountTooSmall,
    #[msg("Total amount must equal the sum of recipient amounts")]
    TotalAmountMismatch,
}
