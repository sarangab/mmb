#![cfg(test)]
use parking_lot::RwLock;
use rust_decimal_macros::dec;

use crate::core::{
    exchanges::binance::binance::Binance, exchanges::common::Amount,
    exchanges::common::CurrencyPair, exchanges::common::ExchangeAccountId,
    exchanges::common::Price, exchanges::events::AllowedEventSourceType,
    exchanges::general::commission::Commission, exchanges::general::commission::CommissionForType,
    exchanges::general::currency_pair_metadata::PrecisionType,
    exchanges::general::features::ExchangeFeatures, exchanges::general::features::OpenOrdersType,
    orders::event::OrderEvent, orders::order::ClientOrderId, orders::order::OrderRole,
    orders::order::OrderSide, orders::order::OrderSnapshot, orders::order::OrderType,
    orders::pool::OrderRef, orders::pool::OrdersPool, settings,
};

use std::sync::{
    mpsc::{channel, Receiver},
    Arc,
};

use super::{currency_pair_metadata::CurrencyPairMetadata, exchange::Exchange};
pub(crate) fn get_test_exchange(is_derivative: bool) -> (Arc<Exchange>, Receiver<OrderEvent>) {
    let exchange_account_id = ExchangeAccountId::new("local_exchange_account_id".into(), 0);
    let settings = settings::ExchangeSettings::new(
        exchange_account_id.clone(),
        "test_api_key".into(),
        "test_secret_key".into(),
        false,
    );

    let binance = Binance::new(settings, "Binance0".parse().expect("in test"));
    let referral_reward = dec!(40);
    let commission = Commission::new(
        CommissionForType::new(dec!(0.1), referral_reward),
        CommissionForType::new(dec!(0.2), referral_reward),
    );

    let (tx, rx) = channel();
    let exchange = Exchange::new(
        exchange_account_id,
        "host".into(),
        vec![],
        vec![],
        Box::new(binance),
        ExchangeFeatures::new(
            OpenOrdersType::AllCurrencyPair,
            false,
            true,
            AllowedEventSourceType::default(),
            AllowedEventSourceType::default(),
        ),
        tx,
        commission,
    );
    let base_currency_code = "PHB";
    let quote_currency_code = "BTC";
    let amount_currency_code = if is_derivative {
        quote_currency_code.clone()
    } else {
        base_currency_code.clone()
    };

    let price_precision = 0;
    let amount_precision = 0;
    let price_tick = dec!(0.1);
    let symbol = CurrencyPairMetadata::new(
        false,
        is_derivative,
        base_currency_code.into(),
        base_currency_code.into(),
        quote_currency_code.into(),
        quote_currency_code.into(),
        None,
        None,
        price_precision,
        PrecisionType::ByFraction,
        Some(price_tick),
        amount_currency_code.into(),
        None,
        None,
        amount_precision,
        PrecisionType::ByFraction,
        None,
        None,
        None,
    );
    exchange
        .symbols
        .insert(symbol.currency_pair(), Arc::new(symbol));

    (exchange, rx)
}

pub(crate) fn create_order_ref(
    client_order_id: &ClientOrderId,
    role: Option<OrderRole>,
    exchange_account_id: &ExchangeAccountId,
    currency_pair: &CurrencyPair,
    price: Price,
    amount: Amount,
    side: OrderSide,
) -> OrderRef {
    let order = OrderSnapshot::with_params(
        client_order_id.clone(),
        OrderType::Liquidation,
        role,
        exchange_account_id.clone(),
        currency_pair.clone(),
        price,
        amount,
        side,
        None,
        "StrategyInUnitTests",
    );

    let order_pool = OrdersPool::new();
    order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
    let order_ref = order_pool
        .cache_by_client_id
        .get(&client_order_id)
        .expect("in test");

    order_ref.clone()
}

pub(crate) fn try_add_snapshot_by_exchange_id(exchange: &Exchange, order_ref: &OrderRef) {
    if let Some(exchange_order_id) = order_ref.exchange_order_id() {
        let _ = exchange
            .orders
            .cache_by_exchange_id
            .insert(exchange_order_id.clone(), order_ref.clone());
    }
}
