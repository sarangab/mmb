use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;

use dashmap::mapref::entry::Entry::{Occupied, Vacant};
use tokio::sync::{broadcast, oneshot};

use crate::core::exchanges::common::{CurrencyCode, ExchangeErrorType};
use crate::core::exchanges::general::currency_pair_metadata::CurrencyPairMetadata;
use crate::core::exchanges::general::exchange::RequestResult;
use crate::core::exchanges::general::features::RestFillsType;
use crate::core::exchanges::general::handlers::handle_order_filled::FillEventData;
use crate::core::exchanges::general::request_type::RequestType;
use crate::core::exchanges::timeouts::requests_timeout_manager::RequestGroupId;
use crate::core::infrastructure::spawn_future_timed;
use crate::core::nothing_to_do;
use crate::core::orders::order::{OrderInfo, OrderStatus};
use crate::core::{
    exchanges::general::exchange::Exchange, lifecycle::cancellation_token::CancellationToken,
    orders::pool::OrderRef,
};

use super::get_order_trades::OrderTrade;

impl Exchange {
    pub async fn wait_order_finish(
        self: Arc<Self>,
        order: &OrderRef,
        pre_reservation_group_id: Option<RequestGroupId>,
        cancellation_token: CancellationToken,
    ) -> Result<OrderRef> {
        // TODO make MetricsRegistry.Metrics.Measure.Timer.Time(MetricsRegistry.Timers.WaitOrderFinishTimer,
        //     MetricsRegistry.Timers.CreateExchangeTimerTags(order.ExchangeId));

        if order.status() == OrderStatus::FailedToCreate {
            return Ok(order.clone());
        }

        match self.wait_finish_order.entry(order.client_order_id()) {
            Occupied(entry) => {
                let tx = entry.get();
                let mut rx = tx.subscribe();
                // Just wait until order cancelling future completed or operation cancelled
                tokio::select! {
                    _ = rx.recv() => nothing_to_do(),
                    _ = cancellation_token.when_cancelled() => nothing_to_do()
                }

                Ok(order.clone())
            }
            Vacant(vacant_entry) => {
                // Be sure value will be removed anyway
                let _guard = scopeguard::guard((), |_| {
                    let _ = self.wait_cancel_order.remove(&order.client_order_id());
                });

                let (tx, _) = broadcast::channel(1);
                let _ = *vacant_entry.insert(tx.clone());

                let outcome = self
                    .clone()
                    .wait_finish_order_work(order, pre_reservation_group_id, cancellation_token)
                    .await?;

                let _ = tx.send(outcome);

                Ok(order.clone())
            }
        }
    }

    pub(crate) async fn wait_finish_order_work(
        self: Arc<Self>,
        order: &OrderRef,
        pre_reservation_group_id: Option<RequestGroupId>,
        cancellation_token: CancellationToken,
    ) -> Result<OrderRef> {
        let has_websocket_notification = self.features.websocket_options.execution_notification;

        if !has_websocket_notification {
            // FIXME implement
            // _polling_trades_counts.add_or_update...
        }

        let linked_cancellation_token = cancellation_token.create_linked_token();

        // if has_websocket_notification: in background we poll for fills every x seconds for those rare cases then we missed a websocket fill
        let cloned_order = order.clone();
        let action = async move {
            self.poll_order_fills(
                &cloned_order,
                has_websocket_notification,
                pre_reservation_group_id,
                linked_cancellation_token,
            )
            .await;
            Ok(())
        };
        let three_hours = Duration::from_secs(10800);
        let poll_order_fill_future = spawn_future_timed(
            "poll_order_fills future",
            false,
            three_hours,
            action.boxed(),
        );

        todo!()
    }

    pub(crate) async fn poll_order_fills(
        &self,
        order: &OrderRef,
        has_websocket_notification: bool,
        pre_reservation_group_id: Option<RequestGroupId>,
        linked_cancellation_token: CancellationToken,
    ) -> () {
        todo!()
    }

    pub(super) async fn check_order_fills(
        &self,
        order: &OrderRef,
        exit_on_order_is_finished_even_if_fills_didnt_received: bool,
        pre_reservation_group_id: Option<RequestGroupId>,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        let currency_pair = order.currency_pair();
        let currency_pair_metadata = self.symbols.get(&currency_pair).with_context(|| {
            format!(
                "No such currency_pair_metadata for given currency_pair {}",
                currency_pair,
            )
        })?;

        let rest_fills_type = &self.features.rest_fills_features.fills_type;
        let request_type_to_use = match rest_fills_type {
            RestFillsType::None => return Ok(()),
            RestFillsType::MyTrades => RequestType::GetOrderTrades,
            RestFillsType::GetOrderInfo => RequestType::GetOrderInfo,
        };

        while !cancellation_token.is_cancellation_requested() {
            if is_finished(
                order,
                exit_on_order_is_finished_even_if_fills_didnt_received,
            ) {
                return Ok(());
            }

            // Sometimes wait_order_finish can be called in fallback before order was created and if creation was slow
            // (i. e. created\failed to create notification message was missed)
            // We end up here before an order was created, so we do not need to check for fills before the moment
            // when Creation fallback does its job and calls created/failed_to_create
            if order.status() == OrderStatus::Creating {
                warn!(
                    "check_order_fills was called for a creating order with client order id {}",
                    order.client_order_id()
                );
                return Ok(());
            }

            order.fn_mut(|order| {
                order.internal_props.last_order_trades_request_time = Some(Utc::now())
            });

            let result = self
                .check_order_fills_using_request_type(
                    order,
                    &currency_pair_metadata,
                    request_type_to_use,
                    pre_reservation_group_id,
                    cancellation_token.clone(),
                )
                .await?;

            match result.get_error() {
                Some(exchange_error) => {
                    if exchange_error.error_type == ExchangeErrorType::OrderNotFound {
                        return Ok(());
                    }

                    warn!("Error received for request_type {:?}, with client_id {}, exchange_order_id {:?}, exchange_account_id {:?}, curency_pair {}: {:?}",
                        request_type_to_use,
                        order.client_order_id(),
                        order.exchange_order_id(),
                        order.exchange_account_id(),
                        order.currency_pair(),
                        exchange_error);

                    // TODO in C# here are placed checking of AAX ServiceUnavailable
                    // with warning and loop breaking
                    // TODO in C# here are placed two temporal hack waiting #598 and #1455 issues implementation
                    // Check all of it later and make better solutions
                }
                None => return Ok(()),
            }
        }

        Ok(())
    }

    pub(crate) async fn check_order_fills_using_request_type(
        &self,
        order: &OrderRef,
        currency_pair_metadata: &CurrencyPairMetadata,
        request_type: RequestType,
        pre_reservation_group_id: Option<RequestGroupId>,
        cancellation_token: CancellationToken,
    ) -> Result<RequestResult<()>> {
        self.timeout_manager
            .reserve_when_available(
                &self.exchange_account_id,
                request_type,
                pre_reservation_group_id,
                cancellation_token,
            )?
            .await;

        info!("Checking request_type {:?} in check_order_fills with client_order_id {}, exchange_order_id {:?}, on {}",
            request_type,
            order.client_order_id(),
            order.exchange_order_id(),
            self.exchange_account_id);

        match request_type {
            RequestType::GetOrderTrades => {
                let order_trades = self.get_order_trades(currency_pair_metadata, order).await?;

                if let RequestResult::Success(ref order_trades) = order_trades {
                    for order_trade in order_trades {
                        if order.get_fills().0.into_iter().any(|order_fill| {
                            order_fill
                                .trade_id()
                                .map(|fill_trade_id| fill_trade_id == &order_trade.trade_id)
                                .unwrap_or(false)
                        }) {
                            continue;
                        };

                        self.handle_order_filled_for_restfallback(order, order_trade)?;
                    }
                }

                match order_trades {
                    RequestResult::Success(_) => Ok(RequestResult::Success(())),
                    RequestResult::Error(error) => Ok(RequestResult::Error(error)),
                }
            }
            RequestType::GetOrderInfo => {
                let order_info = match self.get_order_info(order).await {
                    Ok(order_info) => {
                        let exchange_order_id = order.exchange_order_id().with_context(|| {
                        "No exchange_order_id in order while handle_order_filled_for_restfallback"
                    })?;

                        let commission_currency_code = order_info
                            .commission_currency_code
                            .clone()
                            .map(|currency_code| CurrencyCode::new(currency_code.into()));

                        let event_data = FillEventData {
                            source_type: EventSourceType::RestFallback,
                            trade_id: None,
                            client_order_id: Some(order.client_order_id()),
                            exchange_order_id,
                            fill_price: order_info.average_fill_price,
                            fill_amount: order_info.filled_amount,
                            is_diff: false,
                            total_filled_amount: None,
                            order_role: None,
                            commission_currency_code,
                            commission_rate: order_info.commission_rate,
                            commission_amount: order_info.commission_amount,
                            fill_type: OrderFillType::UserTrade,
                            trade_currency_pair: None,
                            order_side: None,
                            order_amount: None,
                        };
                        self.handle_order_filled(event_data)?;

                        RequestResult::Success(order_info)
                    }
                    Err(exchange_error) => RequestResult::Error::<OrderInfo>(exchange_error),
                };

                match order_info {
                    RequestResult::Success(_) => Ok(RequestResult::Success(())),
                    RequestResult::Error(error) => Ok(RequestResult::Error(error)),
                }
            }
            _ => bail!(
                "Unsupported request type {:?} in check_order_fills",
                request_type
            ),
        }
    }

    pub(crate) fn handle_order_filled_for_restfallback(
        &self,
        order: &OrderRef,
        order_trade: &OrderTrade,
    ) -> Result<()> {
        let exchange_order_id = order.exchange_order_id().ok_or(anyhow!(
            "No exchange_order_id in order while handle_order_filled_for_restfallback"
        ))?;
        let event_data = FillEventData {
            source_type: EventSourceType::RestFallback,
            trade_id: Some(order_trade.trade_id.clone()),
            client_order_id: Some(order.client_order_id()),
            exchange_order_id,
            fill_price: order_trade.price,
            fill_amount: order_trade.amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: Some(order_trade.order_role),
            commission_currency_code: Some(order_trade.fee_currency_code.clone()),
            commission_rate: order_trade.fee_rate,
            commission_amount: order_trade.fee_amount,
            fill_type: OrderFillType::UserTrade,
            trade_currency_pair: None,
            order_side: None,
            order_amount: None,
        };

        self.handle_order_filled(event_data)
    }

    pub(super) async fn create_order_finish_future(
        &self,
        order: &OrderRef,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        if order.is_finished() {
            info!(
                "Instantly exiting create_order_finish_future() because status is {:?} {} {:?} {}",
                order.status(),
                order.client_order_id(),
                order.exchange_order_id(),
                self.exchange_account_id
            );

            return Ok(());
        }

        cancellation_token.error_if_cancellation_requested()?;

        let (tx, rx) = oneshot::channel();
        self.orders_finish_events
            .entry(order.client_order_id())
            .or_insert(tx);

        if order.is_finished() {
            trace!(
                "Exiting create_order_finish_task because order's status turned {:?} {} {:?} {}",
                order.status(),
                order.client_order_id(),
                order.exchange_order_id(),
                self.exchange_account_id
            );

            self.finish_order_future(order);

            return Ok(());
        }

        // Just wait until order cancelling future completed or operation cancelled
        tokio::select! {
            _ = rx => {}
            _ = cancellation_token.when_cancelled() => {}
        }

        Ok(())
    }

    fn finish_order_future(&self, order: &OrderRef) {
        if let Some((_, tx)) = self.orders_finish_events.remove(&order.client_order_id()) {
            let _ = tx.send(());
        }
    }
}

fn is_finished(
    order: &OrderRef,
    exit_on_order_is_finished_even_if_fills_didnt_received: bool,
) -> bool {
    order.status() == OrderStatus::Completed
        || order.is_finished() && exit_on_order_is_finished_even_if_fills_didnt_received
}
