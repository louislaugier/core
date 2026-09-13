use crate::rate::Rate;
use rust_decimal::Decimal;

pub trait LatestRate {
    type Error: std::error::Error + Send + Sync + 'static;

    fn latest_rate(&mut self) -> Result<Rate, Self::Error>;

    /// The spread currently applied on top of the market ask, if this source has one.
    fn current_ask_spread(&self) -> Option<Decimal> {
        None
    }

    /// Apply a new spread without restarting. Sources without a spread ignore it.
    fn set_ask_spread(&mut self, _ask_spread: Decimal) {}
}

// Future: Allow for different price feed sources
pub trait PriceFeed: Sized {
    type Error: std::error::Error + Send + Sync + 'static;
    type Update;

    fn connect(
        url: url::Url,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send;
    fn next_update(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Self::Update, Self::Error>> + Send;
}
