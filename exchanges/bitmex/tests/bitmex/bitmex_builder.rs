use crate::bitmex::common::{get_bitmex_credentials, get_timeout_manager};
use anyhow::{bail, Result};
use bitmex::bitmex::Bitmex;
use mmb_core::balance::manager::balance_manager::BalanceManager;
use mmb_core::database::events::recorder::EventRecorder;
use mmb_core::exchanges::exchange_blocker::ExchangeBlocker;
use mmb_core::exchanges::general::currency_pair_to_symbol_converter::CurrencyPairToSymbolConverter;
use mmb_core::exchanges::general::exchange::Exchange;
use mmb_core::exchanges::general::features::{
    ExchangeFeatures, OpenOrdersType, OrderFeatures, OrderTradeOption, RestFillsFeatures,
    WebSocketOptions,
};
use mmb_core::exchanges::hosts::Hosts;
use mmb_core::exchanges::timeouts::requests_timeout_manager_factory::RequestTimeoutArguments;
use mmb_core::infrastructure::init_lifetime_manager;
use mmb_core::settings::{CurrencyPairSetting, ExchangeSettings};
use mmb_domain::events::{AllowedEventSourceType, ExchangeEvent};
use mmb_domain::exchanges::commission::Commission;
use mmb_domain::market::ExchangeAccountId;
use mmb_domain::order::pool::OrdersPool;
use mmb_domain::order::snapshot::{Amount, Price};
use mmb_utils::cancellation_token::CancellationToken;
use mmb_utils::hashmap;
use mmb_utils::infrastructure::WithExpect;
use std::sync::Arc;
use tokio::sync::broadcast;

// TODO Remove dead code allowing after tests implementation
#[allow(dead_code)]
pub(crate) struct BitmexBuilder {
    pub(crate) exchange: Arc<Exchange>,
    hosts: Hosts,
    exchange_settings: ExchangeSettings,
    pub(crate) default_price: Price,
    pub(crate) min_amount: Amount,
    tx: broadcast::Sender<ExchangeEvent>,
    pub(crate) rx: broadcast::Receiver<ExchangeEvent>,
}

pub const EXCHANGE_ACCOUNT_ID: &str = "Bitmex_0";

impl BitmexBuilder {
    pub(crate) async fn build_account_with_setting(
        settings: ExchangeSettings,
        features: ExchangeFeatures,
        need_to_clean_up: bool,
    ) -> Self {
        BitmexBuilder::try_new_with_settings(
            settings,
            CancellationToken::default(),
            features,
            Commission::default(),
            need_to_clean_up,
        )
        .await
    }

    pub(crate) async fn build_account(need_to_clean_up: bool) -> Result<Self> {
        let exchange_account_id: ExchangeAccountId = EXCHANGE_ACCOUNT_ID.parse().expect("in test");
        BitmexBuilder::try_new(
            exchange_account_id,
            CancellationToken::default(),
            ExchangeFeatures::new(
                OpenOrdersType::AllCurrencyPair,
                RestFillsFeatures::default(),
                OrderFeatures {
                    supports_get_order_info_by_client_order_id: true,
                    ..OrderFeatures::default()
                },
                OrderTradeOption::default(),
                WebSocketOptions::default(),
                true,
                AllowedEventSourceType::default(),
                AllowedEventSourceType::default(),
                AllowedEventSourceType::default(),
            ),
            Commission::default(),
            need_to_clean_up,
            false,
        )
        .await
    }

    pub(crate) async fn build_account_with_source_types(
        allowed_create_event_source_type: AllowedEventSourceType,
        allowed_cancel_event_source_type: AllowedEventSourceType,
        need_to_clean_up: bool,
    ) -> Result<Self> {
        let exchange_account_id: ExchangeAccountId = "Bitmex_0".parse().expect("in test");
        BitmexBuilder::try_new(
            exchange_account_id,
            CancellationToken::default(),
            ExchangeFeatures::new(
                OpenOrdersType::AllCurrencyPair,
                RestFillsFeatures::default(),
                OrderFeatures {
                    supports_get_order_info_by_client_order_id: true,
                    ..OrderFeatures::default()
                },
                OrderTradeOption::default(),
                WebSocketOptions {
                    cancellation_notification: true,
                    ..WebSocketOptions::default()
                },
                true,
                allowed_create_event_source_type,
                AllowedEventSourceType::default(),
                allowed_cancel_event_source_type,
            ),
            Commission::default(),
            need_to_clean_up,
            false,
        )
        .await
    }

    async fn try_new(
        exchange_account_id: ExchangeAccountId,
        cancellation_token: CancellationToken,
        features: ExchangeFeatures,
        commission: Commission,
        need_to_clean_up: bool,
        is_margin_trading: bool,
    ) -> Result<Self> {
        let (api_key, secret_key) = match get_bitmex_credentials() {
            Ok((api_key, secret_key)) => (api_key, secret_key),
            Err(_) => ("".to_string(), "".to_string()),
        };
        if api_key.is_empty() || secret_key.is_empty() {
            bail!(
                "Environment variable BITMEX_SECRET_KEY or BITMEX_API_KEY are not set. Unable to continue test",
            )
        }

        let mut settings = ExchangeSettings::new_short(
            exchange_account_id,
            api_key,
            secret_key,
            is_margin_trading,
        );

        // default currency pair for tests
        settings.currency_pairs = Some(vec![CurrencyPairSetting::Ordinary {
            base: "XBT".into(),
            quote: "USD".into(),
        }]);

        Ok(Self::try_new_with_settings(
            settings,
            cancellation_token,
            features,
            commission,
            need_to_clean_up,
        )
        .await)
    }

    async fn try_new_with_settings(
        settings: ExchangeSettings,
        cancellation_token: CancellationToken,
        features: ExchangeFeatures,
        commission: Commission,
        need_to_clean_up: bool,
    ) -> Self {
        let lifetime_manager = init_lifetime_manager();
        let (tx, rx) = broadcast::channel(10);

        let bitmex = Box::new(Bitmex::new(
            settings.clone(),
            tx.clone(),
            lifetime_manager.clone(),
        ));

        let hosts = bitmex.hosts.clone();

        let exchange_blocker = ExchangeBlocker::new(vec![settings.exchange_account_id]);
        let event_recorder = EventRecorder::start(None, None)
            .await
            .expect("Failure start EventRecorder");

        let timeout_manager = get_timeout_manager(settings.exchange_account_id);
        let exchange = Exchange::new(
            settings.exchange_account_id,
            bitmex,
            OrdersPool::new(),
            features,
            RequestTimeoutArguments::from_requests_per_minute(1200),
            tx.clone(),
            lifetime_manager,
            timeout_manager,
            Arc::downgrade(&exchange_blocker),
            commission,
            event_recorder,
        );
        exchange.build_symbols(&settings.currency_pairs).await;
        exchange.connect_ws().await.with_expect(move || {
            format!(
                "Failed to connect to websockets on exchange {}",
                settings.exchange_account_id
            )
        });

        let currency_pair_to_symbol_converter = CurrencyPairToSymbolConverter::new(
            hashmap![ settings.exchange_account_id => exchange.clone() ],
        );

        let balance_manager = BalanceManager::new(currency_pair_to_symbol_converter, None);

        exchange.setup_balance_manager(balance_manager);

        // TODO Remove that workaround when RAII order clearing will be implemented
        if need_to_clean_up {
            exchange
                .clone()
                .cancel_opened_orders(cancellation_token.clone(), true)
                .await;
        }

        let default_price = 1.into();
        let min_amount = 1.into();

        Self {
            exchange,
            hosts,
            exchange_settings: settings,
            default_price,
            min_amount,
            tx,
            rx,
        }
    }
}
