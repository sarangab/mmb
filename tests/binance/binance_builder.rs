use std::sync::Arc;

use mmb_lib::core::balance_manager::balance_manager::BalanceManager;
use mmb_lib::core::exchanges::common::*;
use mmb_lib::core::exchanges::events::ExchangeEvent;
use mmb_lib::core::exchanges::general::currency_pair_to_metadata_converter::CurrencyPairToMetadataConverter;
use mmb_lib::core::exchanges::general::exchange::*;
use mmb_lib::core::exchanges::general::features::*;
use mmb_lib::core::exchanges::traits::ExchangeClientBuilder;
use mmb_lib::core::exchanges::{binance::binance::*, general::commission::Commission};
use mmb_lib::core::lifecycle::application_manager::ApplicationManager;
use mmb_lib::core::lifecycle::cancellation_token::CancellationToken;
use mmb_lib::core::settings::CurrencyPairSetting;
use mmb_lib::core::settings::ExchangeSettings;

use anyhow::Result;
use mmb_lib::hashmap;
use tokio::sync::broadcast;

use crate::binance::common::{get_binance_credentials, get_timeout_manager};

pub struct BinanceBuilder {
    pub exchange: Arc<Exchange>,
    pub tx: broadcast::Sender<ExchangeEvent>,
    pub rx: broadcast::Receiver<ExchangeEvent>,
}

impl BinanceBuilder {
    pub async fn try_new(
        exchange_account_id: ExchangeAccountId,
        cancellation_token: CancellationToken,
        features: ExchangeFeatures,
        commission: Commission,
        need_to_clean_up: bool,
    ) -> Result<BinanceBuilder> {
        let (api_key, secret_key) = match get_binance_credentials() {
            Ok((api_key, secret_key)) => (api_key, secret_key),
            Err(_) => ("".to_string(), "".to_string()),
        };
        if api_key == "" || secret_key == "" {
            return Err(anyhow::Error::msg(
                "Environment variable BINANCE_SECRET_KEY or BINANCE_API_KEY are not set. Unable to continue test",
            ));
        }

        let mut settings =
            ExchangeSettings::new_short(exchange_account_id, api_key, secret_key, false);

        // default currency pair for tests
        settings.currency_pairs = Some(vec![CurrencyPairSetting {
            base: "cnd".into(),
            quote: "btc".into(),
            currency_pair: None,
        }]);

        BinanceBuilder::try_new_with_settings(
            settings,
            exchange_account_id,
            cancellation_token,
            features,
            commission,
            need_to_clean_up,
        )
        .await
    }

    pub async fn try_new_with_settings(
        mut settings: ExchangeSettings,
        exchange_account_id: ExchangeAccountId,
        cancellation_token: CancellationToken,
        features: ExchangeFeatures,
        commission: Commission,
        need_to_clean_up: bool,
    ) -> Result<BinanceBuilder> {
        let application_manager = ApplicationManager::new(cancellation_token.clone());
        let (tx, rx) = broadcast::channel(10);

        settings.websocket_channels = vec!["depth".into(), "trade".into()];

        let binance = Box::new(Binance::new(
            exchange_account_id,
            settings.clone(),
            tx.clone(),
            application_manager.clone(),
            false,
        ));

        let timeout_manager = get_timeout_manager(exchange_account_id);
        let exchange = Exchange::new(
            exchange_account_id,
            binance,
            features,
            BinanceBuilder.get_timeout_arguments(),
            tx.clone(),
            application_manager,
            timeout_manager,
            commission,
        );
        exchange.clone().connect().await;
        exchange.build_metadata(&settings.currency_pairs).await;

        let currency_pair_to_metadata_converter = CurrencyPairToMetadataConverter::new(
            hashmap![ exchange_account_id => exchange.clone()  ],
        );

        let balance_manager = BalanceManager::new(currency_pair_to_metadata_converter);

        exchange.setup_balance_manager(balance_manager);

        // TODO Remove that workaround when RAII order clearing will be implemented
        if need_to_clean_up {
            exchange
                .clone()
                .cancel_opened_orders(cancellation_token.clone(), true)
                .await;
        }

        Ok(BinanceBuilder { exchange, tx, rx })
    }
}