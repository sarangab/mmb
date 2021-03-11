use super::binance::Binance;
use crate::core::exchanges::traits::Support;
use crate::core::orders::order::*;
use crate::core::{
    exchanges::common::{
        Amount, CurrencyPair, ExchangeErrorType, Price, RestErrorDescription, RestRequestOutcome,
        SpecificCurrencyPair,
    },
    orders::fill::EventSourceType,
};
use async_trait::async_trait;
use itertools::Itertools;
use log::info;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct BinanceOrderInfo {
    #[serde(rename = "symbol")]
    pub specific_currency_pair: SpecificCurrencyPair,
    #[serde(rename = "orderId")]
    pub exchange_order_id: i64, //< local type is ExchangeOrderId
    #[serde(rename = "clientOrderId")]
    pub client_order_id: ClientOrderId,
    pub price: Price,
    #[serde(rename = "origQty")]
    pub orig_quantity: Amount,
    #[serde(rename = "executedQty")]
    pub executed_quantity: Amount,
    pub status: String,
    pub side: String,
}

#[async_trait(?Send)]
impl Support for Binance {
    fn is_rest_error_code(&self, response: &RestRequestOutcome) -> Option<RestErrorDescription> {
        //Binance is a little inconsistent: for failed responses sometimes they include
        //only code or only success:false but sometimes both
        if response.content.contains(r#""success":false"#) || response.content.contains(r#""code""#)
        {
            let data: Value = serde_json::from_str(&response.content).unwrap();
            return Some(RestErrorDescription::new(
                data["msg"].as_str().unwrap().to_owned(),
                data["code"].as_i64().unwrap() as i64,
            ));
        }

        None
    }

    fn get_order_id(&self, response: &RestRequestOutcome) -> ExchangeOrderId {
        let response: Value = serde_json::from_str(&response.content).unwrap();
        let id = response["orderId"].to_string();
        ExchangeOrderId::new(id.into())
    }

    fn get_error_type(&self, error: &RestErrorDescription) -> ExchangeErrorType {
        // -1010 ERROR_MSG_RECEIVED
        // -2010 NEW_ORDER_REJECTED
        // -2011 CANCEL_REJECTED
        match error.message.as_str() {
            "Unknown order sent." | "Order does not exist." => ExchangeErrorType::OrderNotFound,
            "Account has insufficient balance for requested action." => {
                ExchangeErrorType::InsufficientFunds
            }
            "Invalid quantity."
            | "Filter failure: MIN_NOTIONAL"
            | "Filter failure: LOT_SIZE"
            | "Filter failure: PRICE_FILTER"
            | "Filter failure: PERCENT_PRICE"
            | "Quantity less than zero."
            | "Precision is over the maximum defined for this asset." => {
                ExchangeErrorType::InvalidOrder
            }
            msg if msg.contains("Too many requests;") => ExchangeErrorType::RateLimit,
            _ => ExchangeErrorType::Unknown,
        }
    }

    fn on_websocket_message(&self, msg: &str) {
        let data: Value = serde_json::from_str(msg).unwrap();
        // Public stream
        if let Some(stream) = data.get("stream") {
            if stream.as_str().unwrap().contains('@') {
                // TODO handle public stream
            }

            return;
        }

        // so it is userData stream
        let event_type = data["e"].as_str().unwrap();
        if event_type == "executionReport" {
            self.handle_trade(msg, data);
        } else if false {
            // TODO something about ORDER_TRADE_UPDATE? There are no info about it in Binance docs
        } else {
            self.log_unknown_message(self.id.clone(), msg);
        }
    }

    fn set_order_created_callback(
        &self,
        callback: Box<dyn FnMut(ClientOrderId, ExchangeOrderId, EventSourceType)>,
    ) {
        *self.order_created_callback.lock() = callback;
    }

    fn set_order_cancelled_callback(
        &self,
        callback: Box<dyn FnMut(ClientOrderId, ExchangeOrderId, EventSourceType)>,
    ) {
        *self.order_cancelled_callback.lock() = callback;
    }

    fn build_ws_main_path(
        &self,
        specific_currency_pairs: &[SpecificCurrencyPair],
        websocket_channels: &[String],
    ) -> String {
        let stream_names = specific_currency_pairs
            .iter()
            .flat_map(|currency_pair| {
                let mut results = Vec::new();
                for channel in websocket_channels {
                    let result = Self::get_stream_name(currency_pair, channel);
                    results.push(result);
                }
                results
            })
            .join("/");
        let ws_path = format!("/stream?streams={}", stream_names);
        ws_path.to_lowercase()
    }

    async fn build_ws_secondary_path(&self) -> String {
        let request_outcome = self.get_listen_key().await;
        let data: Value = serde_json::from_str(&request_outcome.content).unwrap();
        let listen_key = data["listenKey"].as_str().unwrap().to_owned();

        let ws_path = format!("{}{}", "/ws/", listen_key);
        ws_path
    }

    fn should_log_message(&self, message: &str) -> bool {
        message.contains("executionReporn")
    }

    fn get_specific_currency_pair(&self, currency_pair: &CurrencyPair) -> SpecificCurrencyPair {
        self.unified_to_specific[currency_pair].clone()
    }

    fn parse_open_orders(&self, response: &RestRequestOutcome) -> Vec<OrderInfo> {
        // TODO that unwrap has to be just logging
        let binance_orders: Vec<BinanceOrderInfo> =
            serde_json::from_str(&response.content).unwrap();

        let orders_info: Vec<OrderInfo> = binance_orders
            .iter()
            .map(|order| self.specific_order_info_to_unified(order))
            .collect();

        orders_info
    }

    fn log_unknown_message(
        &self,
        exchange_account_id: crate::core::exchanges::common::ExchangeAccountId,
        message: &str,
    ) {
        info!("Unknown message for {}: {}", exchange_account_id, message);
    }
}