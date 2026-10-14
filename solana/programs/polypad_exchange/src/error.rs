use anchor_lang::prelude::*;

/// Why a quote was refused (`quote::verify`). A plain enum so the quote
/// module's checks stay pure and testable; it maps one to one onto the first
/// codes of `ExchangeError`, the program's only error enum (an Anchor IDL
/// allows one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteError {
    WrongSide,
    QuoteExpired,
    QuoteTooLong,
    PriceOutOfRange,
    NotEd25519,
    BadEd25519Header,
    BadSignature,
}

impl From<QuoteError> for ExchangeError {
    fn from(e: QuoteError) -> Self {
        match e {
            QuoteError::WrongSide => ExchangeError::WrongSide,
            QuoteError::QuoteExpired => ExchangeError::QuoteExpired,
            QuoteError::QuoteTooLong => ExchangeError::QuoteTooLong,
            QuoteError::PriceOutOfRange => ExchangeError::PriceOutOfRange,
            QuoteError::NotEd25519 => ExchangeError::NotEd25519,
            QuoteError::BadEd25519Header => ExchangeError::BadEd25519Header,
            QuoteError::BadSignature => ExchangeError::BadSignature,
        }
    }
}

impl From<QuoteError> for anchor_lang::error::Error {
    fn from(e: QuoteError) -> Self {
        ExchangeError::from(e).into()
    }
}

/// Every error the program returns, named as in `PriceOracle` and `PExchange`.
#[error_code]
#[derive(PartialEq, Eq)]
pub enum ExchangeError {
    #[msg("quote is for the other side")]
    WrongSide,
    #[msg("quote expired")]
    QuoteExpired,
    #[msg("quote valid for longer than MAX_VALIDITY")]
    QuoteTooLong,
    #[msg("price outside (0, MAX_PRICE] for a quote, or above $1 for a payout")]
    PriceOutOfRange,
    #[msg("no ed25519 verification right before this instruction")]
    NotEd25519,
    #[msg("ed25519 instruction is malformed or reads outside its own data")]
    BadEd25519Header,
    #[msg("quote not signed by the oracle signer")]
    BadSignature,
    #[msg("only the owner")]
    OnlyOwner,
    #[msg("only the keeper")]
    OnlyKeeper,
    #[msg("only the factory or the owner")]
    OnlyFactory,
    #[msg("only the pending owner")]
    NotPendingOwner,
    #[msg("bad parameters")]
    BadParams,
    #[msg("nothing pending")]
    NothingPending,
    #[msg("not yet")]
    NotYet,
    #[msg("halted")]
    Halted,
    #[msg("zero amount")]
    ZeroAmount,
    #[msg("quote is for another market")]
    QuoteForOtherMarket,
    #[msg("quote does not cover this size")]
    QuoteTooSmall,
    #[msg("price outside the band")]
    PriceOutOfBand,
    #[msg("unbacked risk over the cap")]
    RiskCap,
    #[msg("not enough free float")]
    InsufficientFloat,
    #[msg("slippage")]
    Slippage,
    #[msg("no bridge deposit set")]
    NoBridge,
    #[msg("the bridge deposit changes through propose/accept")]
    BridgeChangeDelayed,
    #[msg("the float and pTokens cannot be rescued")]
    NotRescuable,
    #[msg("market settled at $0: nothing to mint")]
    SettledAtZero,
    #[msg("under MIN_DELAYED over a spent cap")]
    DustOverCap,
    #[msg("market already settled")]
    AlreadySettled,
    #[msg("market paused")]
    MarketPaused,
    #[msg("market settled")]
    MarketSettled,
    #[msg("no settlement pays here: the market is open")]
    NotAtPayout,
    #[msg("no oracle signer")]
    NoSigner,
    #[msg("an account this instruction needs was not passed")]
    MissingAccount,
    #[msg("an account is not the one expected")]
    WrongAccount,
    #[msg("queue accounts out of order")]
    QueueOutOfOrder,
    #[msg("quote fill still live")]
    FillLive,
    #[msg("the float is frozen: no payment can be made")]
    FloatFrozen,
    #[msg("arithmetic overflow")]
    MathOverflow,
}
