use anchor_lang::prelude::*;
use polypad_exchange::error::QuoteError;

/// Every error the program returns. The first seven are the exchange's quote
/// errors under the same names: `launch` runs the exchange's own quote check.
#[error_code]
#[derive(PartialEq, Eq)]
pub enum LaunchError {
    #[msg("quote is for the other side")]
    WrongSide,
    #[msg("quote expired")]
    QuoteExpired,
    #[msg("quote valid for longer than MAX_VALIDITY")]
    QuoteTooLong,
    #[msg("price outside (0, MAX_PRICE]")]
    PriceOutOfRange,
    #[msg("no ed25519 verification right before this instruction")]
    NotEd25519,
    #[msg("ed25519 instruction is malformed or reads outside its own data")]
    BadEd25519Header,
    #[msg("quote not signed by the oracle signer")]
    BadSignature,
    #[msg("only the owner")]
    OnlyOwner,
    #[msg("only the pending owner")]
    NotPendingOwner,
    #[msg("only the keeper")]
    OnlyKeeper,
    #[msg("only the payee")]
    OnlyPayee,
    #[msg("only the pending payee")]
    NotPendingPayee,
    #[msg("bad parameters")]
    BadParams,
    #[msg("bad fees")]
    BadFees,
    #[msg("holders share above 100%")]
    BadHoldersShare,
    #[msg("quote is for another market")]
    QuoteForOtherMarket,
    #[msg("no oracle signer")]
    NoSigner,
    #[msg("price outside the exchange's band")]
    PriceOutOfBand,
    #[msg("market paused")]
    MarketPaused,
    #[msg("market settled")]
    MarketSettled,
    #[msg("launch must be a top-level instruction")]
    NotTopLevel,
    #[msg("DBC config is not what launch asked for")]
    ConfigMismatch,
    #[msg("an account this path needs is missing")]
    MissingAccount,
    #[msg("an account is not the one expected")]
    WrongAccount,
    #[msg("the curve has not completed")]
    NotMigrated,
    #[msg("already graduated")]
    AlreadyGraduated,
    #[msg("the pool position is not locked and owned by the fee vault")]
    NotLocked,
    #[msg("nothing to withdraw")]
    NothingToClaim,
    #[msg("arithmetic overflow")]
    MathOverflow,
    #[msg("the pool was opened after the launch quote expired")]
    PoolTooLate,
    #[msg("the launch's pool was opened in time")]
    NotExpired,
    #[msg("this pool is a Polypad coin: its fees go through collect")]
    IsCoin,
}

impl From<QuoteError> for LaunchError {
    fn from(e: QuoteError) -> Self {
        match e {
            QuoteError::WrongSide => LaunchError::WrongSide,
            QuoteError::QuoteExpired => LaunchError::QuoteExpired,
            QuoteError::QuoteTooLong => LaunchError::QuoteTooLong,
            QuoteError::PriceOutOfRange => LaunchError::PriceOutOfRange,
            QuoteError::NotEd25519 => LaunchError::NotEd25519,
            QuoteError::BadEd25519Header => LaunchError::BadEd25519Header,
            QuoteError::BadSignature => LaunchError::BadSignature,
        }
    }
}
