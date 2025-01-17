use crate::types::{BitmexOrderInfo, BitmexSymbol};
use anyhow::{Context, Result};
use arrayvec::{ArrayString, ArrayVec};
use dashmap::DashMap;
use function_name::named;
use hmac::{Hmac, Mac};
use hyper::http::request::Builder;
use hyper::{StatusCode, Uri};
use mmb_core::exchanges::general::features::{
    BalancePositionOption, ExchangeFeatures, OpenOrdersType, OrderFeatures, OrderTradeOption,
    RestFillsFeatures, RestFillsType, WebSocketOptions,
};
use mmb_core::exchanges::hosts::Hosts;
use mmb_core::exchanges::rest_client::{
    ErrorHandler, ErrorHandlerData, RequestType, RestClient, RestHeaders, RestResponse, UriBuilder,
};
use mmb_core::exchanges::timeouts::requests_timeout_manager_factory::RequestTimeoutArguments;
use mmb_core::exchanges::timeouts::timeout_manager::TimeoutManager;
use mmb_core::exchanges::traits::{
    ExchangeClientBuilder, ExchangeClientBuilderResult, ExchangeError, HandleOrderFilledCb,
    HandleTradeCb, OrderCancelledCb, OrderCreatedCb, SendWebsocketMessageCb, Support,
};
use mmb_core::lifecycle::app_lifetime_manager::AppLifetimeManager;
use mmb_core::settings::ExchangeSettings;
use mmb_domain::events::{AllowedEventSourceType, ExchangeEvent};
use mmb_domain::exchanges::symbol::{Precision, Symbol};
use mmb_domain::market::{
    CurrencyCode, CurrencyId, CurrencyPair, ExchangeErrorType, ExchangeId, SpecificCurrencyPair,
};
use mmb_domain::order::pool::{OrderRef, OrdersPool};
use mmb_domain::order::snapshot::{
    ExchangeOrderId, OrderCancelling, OrderExecutionType, OrderInfo, OrderSide, OrderStatus,
    OrderType, Price,
};
use parking_lot::{Mutex, RwLock};
use rust_decimal_macros::dec;
use serde::Deserialize;
use sha2::Sha256;
use std::collections::HashMap;
use std::fmt;
use std::fmt::{Display, Formatter};
use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tinyvec::Array;
use tokio::sync::broadcast;
use urlencoding::encode;

#[derive(Default)]
pub struct ErrorHandlerBitmex;

impl ErrorHandler for ErrorHandlerBitmex {
    fn check_spec_rest_error(&self, response: &RestResponse) -> Result<(), ExchangeError> {
        match response.status {
            // StatusCode::UNAUTHORIZED is possible too but it is handling in RestClient::get_rest_error()
            StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                Err(ExchangeError::unknown(&response.content))
            }
            _ => Ok(()),
        }
    }

    fn clarify_error_type(&self, _error: &ExchangeError) -> ExchangeErrorType {
        ExchangeErrorType::Unknown
    }
}

struct RestHeadersBitmex {
    api_key: String,
    secret_key: String,
}

impl RestHeadersBitmex {
    pub fn new(api_key: String, secret_key: String) -> Self {
        Self {
            api_key,
            secret_key,
        }
    }

    pub fn create_signature_message(
        path_and_query: &str,
        request_type: RequestType,
    ) -> (ArrayString<128>, u64) {
        let mut message = ArrayString::<128>::new();
        message.push_str(request_type.as_str());
        message.push_str(path_and_query);

        let expire_time = Bitmex::get_key_expire_time(60);

        (message, expire_time)
    }
}

impl RestHeaders for RestHeadersBitmex {
    fn add_specific_headers(
        &self,
        builder: Builder,
        uri: &Uri,
        request_type: RequestType,
    ) -> Builder {
        let path_and_query = match uri.path_and_query() {
            Some(path_and_query) => path_and_query.as_str(),
            None => uri.path(),
        };
        let (message, expire_time) =
            RestHeadersBitmex::create_signature_message(path_and_query, request_type);

        builder
            .header("api-expires", expire_time)
            .header("api-key", &self.api_key)
            .header(
                "api-signature",
                Bitmex::create_signature(&self.secret_key, message.as_str(), expire_time)
                    .as_slice(),
            )
    }
}

const EMPTY_RESPONSE_IS_OK: bool = false;

pub struct Bitmex {
    pub(crate) settings: ExchangeSettings,
    pub hosts: Hosts,
    rest_client: RestClient<ErrorHandlerBitmex, RestHeadersBitmex>,
    pub(crate) unified_to_specific: RwLock<HashMap<CurrencyPair, SpecificCurrencyPair>>,
    specific_to_unified: RwLock<HashMap<SpecificCurrencyPair, CurrencyPair>>,
    pub(crate) supported_currencies: DashMap<CurrencyId, CurrencyCode>,
    // Currencies used for trading according to user settings
    pub(super) traded_specific_currencies: Mutex<Vec<SpecificCurrencyPair>>,
    pub(super) lifetime_manager: Arc<AppLifetimeManager>,
    pub(super) events_channel: broadcast::Sender<ExchangeEvent>,
    pub(crate) order_created_callback: OrderCreatedCb,
    pub(crate) order_cancelled_callback: OrderCancelledCb,
    pub(crate) handle_order_filled_callback: HandleOrderFilledCb,
    pub(crate) handle_trade_callback: HandleTradeCb,
    pub(crate) websocket_message_callback: SendWebsocketMessageCb,
    pub(super) order_book_ids: Mutex<HashMap<(SpecificCurrencyPair, u64), Price>>,
}

impl Bitmex {
    pub fn new(
        settings: ExchangeSettings,
        events_channel: broadcast::Sender<ExchangeEvent>,
        lifetime_manager: Arc<AppLifetimeManager>,
    ) -> Bitmex {
        Self {
            rest_client: RestClient::new(
                ErrorHandlerData::new(
                    EMPTY_RESPONSE_IS_OK,
                    settings.exchange_account_id,
                    ErrorHandlerBitmex::default(),
                ),
                RestHeadersBitmex::new(settings.api_key.clone(), settings.secret_key.clone()),
            ),
            settings,
            hosts: Self::make_hosts(),
            unified_to_specific: Default::default(),
            specific_to_unified: Default::default(),
            supported_currencies: Default::default(),
            traded_specific_currencies: Default::default(),
            events_channel,
            lifetime_manager,
            order_created_callback: Box::new(|_, _, _| {}),
            order_cancelled_callback: Box::new(|_, _, _| {}),
            handle_order_filled_callback: Box::new(|_| {}),
            handle_trade_callback: Box::new(|_, _| {}),
            websocket_message_callback: Box::new(|_, _| Ok(())),
            order_book_ids: Default::default(),
        }
    }

    fn make_hosts() -> Hosts {
        Hosts {
            web_socket_host: "wss://www.bitmex.com/realtime",
            web_socket2_host: "wss://www.bitmex.com/realtime",
            rest_host: "https://www.bitmex.com",
        }
    }

    #[named]
    pub(super) async fn request_all_symbols(&self) -> Result<RestResponse, ExchangeError> {
        let builder = UriBuilder::from_path("/api/v1/instrument/activeAndIndices");
        let uri = builder.build_uri(self.hosts.rest_uri_host(), false);

        self.rest_client
            .get(uri, function_name!(), "".to_string())
            .await
    }

    pub(super) fn parse_all_symbols(&self, response: &RestResponse) -> Result<Vec<Arc<Symbol>>> {
        let symbols: Vec<BitmexSymbol> = serde_json::from_str(&response.content)
            .expect("Unable to deserialize response from Bitmex");
        let mut supported_symbols = Vec::new();

        for symbol in &symbols {
            if Bitmex::is_unsupported_symbol(symbol) {
                continue;
            }

            let base = symbol.base_id.into();
            let quote = symbol.quote_id.into();

            let specific_currency_pair = symbol.id.into();
            let unified_currency_pair = CurrencyPair::from_codes(base, quote);
            self.unified_to_specific
                .write()
                .insert(unified_currency_pair, specific_currency_pair);

            self.specific_to_unified
                .write()
                .insert(specific_currency_pair, unified_currency_pair);

            let (amount_currency_code, balance_currency_code) = if symbol.id != "XBTUSD" {
                (base, None)
            } else {
                (CurrencyCode::from("XBT"), Some(CurrencyCode::from("BTC")))
            };

            let price_tick = symbol.price_tick.expect("Null price tick value");
            let amount_tick = symbol.amount_tick.expect("Null amount tick value");

            let symbol = Symbol::new(
                self.settings.is_margin_trading,
                symbol.base_id.into(),
                base,
                symbol.quote_id.into(),
                quote,
                None,
                symbol.max_price,
                None,
                symbol.max_amount,
                None,
                amount_currency_code,
                balance_currency_code,
                Precision::ByTick { tick: price_tick },
                Precision::ByTick { tick: amount_tick },
            );

            supported_symbols.push(Arc::new(symbol));
        }

        Ok(supported_symbols)
    }

    fn is_unsupported_symbol(symbol: &BitmexSymbol) -> bool {
        let is_inactive_symbol = symbol.state == "Unlisted";

        // Symbols list has the same CurrencyCodePair for all the BTC/USD futures, we keep only perpetual swap for now
        let is_unsupported_futures = (symbol.base_id == "XBT" || symbol.base_id == "BTC")
            && symbol.quote_id == "USD"
            && symbol.id != "XBTUSD";

        is_inactive_symbol || is_unsupported_futures
    }

    #[named]
    pub(super) async fn do_create_order(
        &self,
        order: &OrderRef,
    ) -> Result<RestResponse, ExchangeError> {
        let (header, price, stop_loss_price, mut trailing_stop_delta) = order.fn_ref(|order| {
            (
                order.header.clone(),
                order.price(),
                order.props.stop_loss_price,
                order.props.trailing_stop_delta,
            )
        });
        let specific_currency_pair = self.get_specific_currency_pair(header.currency_pair);

        let mut builder = UriBuilder::from_path("/api/v1/order");
        builder.add_kv("symbol", specific_currency_pair);
        builder.add_kv("side", header.side.as_str());
        builder.add_kv("orderQty", header.amount);
        builder.add_kv("clOrdID", header.client_order_id.as_str());

        match header.order_type {
            OrderType::Market => builder.add_kv("ordType", "Market"),
            OrderType::Limit => {
                builder.add_kv("ordType", "Limit");
                builder.add_kv("price", price);
                if header.execution_type == OrderExecutionType::MakerOnly {
                    builder.add_kv("execInst", "ParticipateDoNotInitiate");
                }
            }
            OrderType::StopLoss => {
                builder.add_kv("ordType", "Stop");
                builder.add_kv("stopPx", stop_loss_price);
            }
            OrderType::TrailingStop => {
                builder.add_kv("ordType", "Stop");
                builder.add_kv("pegPriceType", "TrailingStopPeg");
                if header.side == OrderSide::Sell {
                    trailing_stop_delta.set_sign_negative(true);
                }
                builder.add_kv("pegOffsetValue", trailing_stop_delta);
            }
            OrderType::ClosePosition => {
                // It will cancel other active limit orders with the same side and symbol if the open quantity exceeds the current position
                // Details: https://www.bitmex.com/api/explorer/#!/Order/Order_new
                builder.add_kv("ordType", "Close");
            }
            _ => return Err(ExchangeError::unknown("Unexpected order type")),
        }

        let uri = builder.build_uri(self.hosts.rest_uri_host(), true);
        let log_args = format!("Create order for {header:?}");
        self.rest_client
            .post(uri, None, function_name!(), log_args)
            .await
    }

    pub(super) fn get_order_id(
        &self,
        response: &RestResponse,
    ) -> Result<ExchangeOrderId, ExchangeError> {
        #[derive(Deserialize)]
        struct OrderId<'a> {
            #[serde(rename = "orderID")]
            order_id: &'a str,
        }

        let deserialized: OrderId = serde_json::from_str(&response.content)
            .map_err(|err| ExchangeError::parsing(format!("Unable to parse orderId: {err:?}")))?;

        Ok(ExchangeOrderId::from(deserialized.order_id))
    }

    #[named]
    pub(super) async fn request_open_orders(
        &self,
        currency_pair: Option<CurrencyPair>,
    ) -> Result<RestResponse, ExchangeError> {
        let mut builder = UriBuilder::from_path("/api/v1/order");
        struct OpenOrdersFilter {
            specific_currency_pair: Option<SpecificCurrencyPair>,
        }
        impl Display for OpenOrdersFilter {
            fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                // TODO Possible write a serializer to JSON format
                // Creating JSON without heap allocation
                let mut string_data = ArrayString::<64>::new();
                string_data.push('{');
                string_data.push_str(r#""open": true"#);
                if let Some(specific_currency_pair) = self.specific_currency_pair {
                    string_data.push_str(", ");
                    string_data.push_str(r#""symbol": ""#);
                    string_data.push_str(specific_currency_pair.as_str());
                    string_data.push('"');
                }
                string_data.push('}');

                write!(f, "{}", encode(string_data.as_str()))
            }
        }

        let filter = match currency_pair {
            Some(pair) => OpenOrdersFilter {
                specific_currency_pair: Some(self.get_specific_currency_pair(pair)),
            },
            None => OpenOrdersFilter {
                specific_currency_pair: None,
            },
        };

        builder.add_kv("filter", filter);

        let uri = builder.build_uri(self.hosts.rest_uri_host(), true);
        self.rest_client
            .get(uri, function_name!(), "".to_string())
            .await
    }

    pub(super) fn parse_open_orders(&self, response: &RestResponse) -> Result<Vec<OrderInfo>> {
        let bitmex_orders: Vec<BitmexOrderInfo> = serde_json::from_str(&response.content)
            .context("Unable to parse response content for get_open_orders request")?;

        Ok(bitmex_orders
            .iter()
            .map(|order| self.specific_order_info_to_unified(order))
            .collect())
    }

    fn specific_order_info_to_unified(&self, specific: &BitmexOrderInfo) -> OrderInfo {
        let price = match specific.price {
            Some(price) => price,
            None => dec!(0),
        };
        let average_price = match specific.average_fill_price {
            Some(price) => price,
            None => dec!(0),
        };
        let amount = match specific.amount {
            Some(amount) => amount,
            None => dec!(0),
        };
        let filled_amount = match specific.filled_amount {
            Some(amount) => amount,
            None => dec!(0),
        };
        OrderInfo::new(
            self.get_unified_currency_pair(&specific.specific_currency_pair)
                .expect("Expected known currency pair"),
            specific.exchange_order_id.clone(),
            specific.client_order_id.clone(),
            specific.side,
            Bitmex::get_local_order_status(specific.status),
            price,
            amount,
            average_price,
            filled_amount,
            // Bitmex doesn't return commission info on GET /order request
            None,
            None,
            None,
        )
    }

    pub(super) fn get_unified_currency_pair(
        &self,
        currency_pair: &SpecificCurrencyPair,
    ) -> Result<CurrencyPair> {
        self.specific_to_unified
            .read()
            .get(currency_pair)
            .cloned()
            .with_context(|| {
                format!(
                    "Not found currency pair '{currency_pair:?}' in {}",
                    self.settings.exchange_account_id
                )
            })
    }

    pub(super) fn get_local_order_status(status: &str) -> OrderStatus {
        match status {
            "New" | "PartiallyFilled" => OrderStatus::Created,
            "Filled" => OrderStatus::Completed,
            "Canceled" | "Expired" | "Stopped" => OrderStatus::Canceled,
            "Rejected" => OrderStatus::FailedToCreate,
            _ => panic!("Bitmex: unexpected order status {}", status),
        }
    }

    #[named]
    pub(super) async fn request_order_info(
        &self,
        order: &OrderRef,
    ) -> Result<RestResponse, ExchangeError> {
        let client_order_id = order.client_order_id();

        let mut builder = UriBuilder::from_path("/api/v1/order");
        let mut filter_string = ArrayString::<64>::new();
        fmt::write(
            &mut filter_string,
            format_args!(r#"{{"clOrdID": "{}"}}"#, client_order_id.as_str()),
        )
        .expect("Failed to create filter string");
        builder.add_kv("filter", encode(filter_string.as_str()));

        let uri = builder.build_uri(self.hosts.rest_uri_host(), true);
        let log_args = format!("order {client_order_id}");

        self.rest_client.get(uri, function_name!(), log_args).await
    }

    pub(super) fn parse_order_info(&self, response: &RestResponse) -> Result<OrderInfo> {
        let specific_orders: Vec<BitmexOrderInfo> = serde_json::from_str(&response.content)
            .context("Unable to parse response content for get_order_info request")?;

        let order = specific_orders
            .first()
            .context("No one order info received")?;

        Ok(self.specific_order_info_to_unified(order))
    }

    #[named]
    pub(super) async fn do_cancel_order(
        &self,
        order: OrderCancelling,
    ) -> Result<RestResponse, ExchangeError> {
        let mut builder = UriBuilder::from_path("/api/v1/order");
        // Order may be canceled passing either exchange_order_id ("orderID" key) or client_order_id ("clOrdID" key)
        builder.add_kv("orderID", &order.exchange_order_id);

        let uri = builder.build_uri(self.hosts.rest_uri_host(), true);
        let log_args = format!("Cancel order for {}", order.header.client_order_id);

        self.rest_client
            .delete(uri, function_name!(), log_args)
            .await
    }

    #[named]
    pub(super) async fn do_cancel_all_orders(&self) -> Result<RestResponse, ExchangeError> {
        let builder = UriBuilder::from_path("/api/v1/order/all");

        let uri = builder.build_uri(self.hosts.rest_uri_host(), true);
        let log_args = "Cancel all orders".to_owned();

        self.rest_client
            .delete(uri, function_name!(), log_args)
            .await
    }

    pub(super) fn create_signature(secret_key: &str, message: &str, expire_time: u64) -> [u8; 64] {
        let mut hmac = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes())
            .expect("Unable to calculate hmac for Bitmex signature");
        hmac.update(message.as_bytes());

        let mut expire_time_array = ArrayVec::<u8, 20>::new();
        write!(expire_time_array, "{expire_time}").expect("Failed to convert UNIX time to string");
        hmac.update(expire_time_array.as_slice());

        let hmac_bytes = hmac.finalize().into_bytes();

        let mut hex_array = [0u8; 64];
        write!(hex_array.as_slice_mut(), "{:x}", hmac_bytes)
            .expect("Failed to convert signature bytes array to hex");

        hex_array
    }

    pub(super) fn get_key_expire_time(secs: u64) -> u64 {
        let current_unix_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System Time before UNIX EPOCH!")
            .as_secs();

        current_unix_time + secs
    }
}

pub struct BitmexBuilder;

impl ExchangeClientBuilder for BitmexBuilder {
    fn create_exchange_client(
        &self,
        exchange_settings: ExchangeSettings,
        events_channel: broadcast::Sender<ExchangeEvent>,
        lifetime_manager: Arc<AppLifetimeManager>,
        _timeout_manager: Arc<TimeoutManager>,
        _orders: Arc<OrdersPool>,
    ) -> ExchangeClientBuilderResult {
        ExchangeClientBuilderResult {
            client: Box::new(Bitmex::new(
                exchange_settings,
                events_channel,
                lifetime_manager,
            )),
            features: ExchangeFeatures {
                open_orders_type: OpenOrdersType::AllCurrencyPair,
                rest_fills_features: RestFillsFeatures::new(RestFillsType::MyTrades),
                order_features: OrderFeatures {
                    maker_only: true,
                    supports_get_order_info_by_client_order_id: true,
                    cancellation_response_from_rest_only_for_errors: true,
                    creation_response_from_rest_only_for_errors: true,
                    order_was_completed_error_for_cancellation: true,
                    supports_already_cancelled_order: true,
                    supports_stop_loss_order: true,
                },
                trade_option: OrderTradeOption {
                    supports_trade_time: true,
                    supports_trade_incremented_id: false,
                    notification_on_each_currency_pair: false,
                    supports_get_prints: true,
                    supports_tick_direction: true,
                    supports_my_trades_from_time: true,
                },
                websocket_options: WebSocketOptions {
                    execution_notification: true,
                    cancellation_notification: true,
                    supports_ping_pong: true,
                    supports_subscription_response: false,
                },
                empty_response_is_ok: EMPTY_RESPONSE_IS_OK,
                balance_position_option: BalancePositionOption::NonDerivative,
                allowed_create_event_source_type: AllowedEventSourceType::All,
                allowed_fill_event_source_type: AllowedEventSourceType::All,
                allowed_cancel_event_source_type: AllowedEventSourceType::All,
            },
        }
    }

    fn get_timeout_arguments(&self) -> RequestTimeoutArguments {
        RequestTimeoutArguments::from_requests_per_minute(60)
    }

    fn get_exchange_id(&self) -> ExchangeId {
        "Bitmex".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bstr::ByteSlice;

    #[test]
    fn generate_signature() {
        // Test data from https://www.bitmex.com/app/apiKeysUsage
        let api_key = "LAqUlngMIQkIUjXMUreyu3qn".to_owned();
        let secret_key = "chNOOS4KvNXR_Xq4k4c9qsfoKWvnDecLATCRlcBwyKDYnWgO".to_owned();
        let path = "/api/v1/instrument?filter=%7B%22symbol%22%3A+%22XBTM15%22%7D";
        let expire_time = 1518064237;

        let rest_header = RestHeadersBitmex {
            api_key,
            secret_key,
        };

        let (message, _) = RestHeadersBitmex::create_signature_message(path, RequestType::Get);

        let signature_hash =
            Bitmex::create_signature(&rest_header.secret_key, message.as_str(), expire_time);

        assert_eq!(
            signature_hash
                .to_str()
                .expect("Failed to convert signature hash to string"),
            "e2f422547eecb5b3cb29ade2127e21b858b235b386bfa45e1c1756eb3383919f"
        );
    }
}
