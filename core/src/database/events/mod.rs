use crate::infrastructure::spawn_future;
use crate::lifecycle::trading_engine::Service;
use anyhow::{Context, Result};
use mmb_database::postgres_db;
use mmb_database::postgres_db::events::{
    save_events_batch, save_events_one_by_one, Event, InsertEvent, TableName,
};
use mmb_database::postgres_db::Client;
use mmb_utils::infrastructure::SpawnFutureFlags;
use mmb_utils::logger::print_info;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::mem;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const BATCH_MAX_SIZE: usize = 65_536;
const BATCH_SIZE_TO_SAVE: usize = 250;
const SAVE_TIMEOUT: Duration = Duration::from_secs(1);

pub struct EventRecorder {
    data_tx: mpsc::Sender<(InsertEvent, TableName)>,
    shutdown_signal_tx: mpsc::UnboundedSender<()>,
    shutdown_rx: Mutex<Option<oneshot::Receiver<Result<()>>>>,
}

impl EventRecorder {
    pub fn start(database_url: Option<String>) -> Arc<EventRecorder> {
        let (data_tx, data_rx) = mpsc::channel(20_000);
        let (shutdown_signal_tx, shutdown_signal_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        match database_url {
            None => {
                let _ = shutdown_tx.send(Ok(()));
                print_info(
                    "EventRecorder is not started because `database_url` is not set in settings",
                )
            }
            Some(database_url) => {
                let _ = spawn_future(
                    "start db event recorder",
                    SpawnFutureFlags::DENY_CANCELLATION | SpawnFutureFlags::STOP_BY_TOKEN,
                    start_db_event_recorder(database_url, data_rx, shutdown_signal_rx, shutdown_tx),
                );
                print_info("EventRecorder started");
            }
        }

        Arc::new(Self {
            data_tx,
            shutdown_signal_tx,
            shutdown_rx: Mutex::new(Some(shutdown_rx)),
        })
    }

    pub fn save(&self, event: impl Event) -> Result<()> {
        let table_name = event.get_table_name();

        if !self.data_tx.is_closed() {
            self.data_tx
                .try_send((
                    InsertEvent {
                        version: event.get_version(),
                        json: event
                            .get_json()
                            .context("serialization to json in `EventRecorder::save()`")?,
                    },
                    table_name,
                ))
                .context("failed EventRecorder::save()")?
        }

        Ok(())
    }
}

impl Service for EventRecorder {
    fn name(&self) -> &str {
        "EventRecorder"
    }

    fn graceful_shutdown(self: Arc<Self>) -> Option<oneshot::Receiver<Result<()>>> {
        let _ = self.shutdown_signal_tx.send(());

        self.shutdown_rx.lock().take()
    }
}

async fn start_db_event_recorder(
    database_url: String,
    mut data_rx: mpsc::Receiver<(InsertEvent, TableName)>,
    mut shutdown_signal_rx: mpsc::UnboundedReceiver<()>,
    shutdown_tx: oneshot::Sender<Result<()>>,
) -> Result<()> {
    let (mut client, connection) =
        postgres_db::connect(&database_url).await.with_context(|| {
            format!("from `start_db_event_recorder` with connection_string: {database_url}")
        })?;

    let _ = spawn_future(
        "Db connection handler",
        SpawnFutureFlags::DENY_CANCELLATION | SpawnFutureFlags::STOP_BY_TOKEN,
        connection.handle(),
    );

    fn create_batch_size_vec() -> Vec<InsertEvent> {
        Vec::<InsertEvent>::with_capacity(BATCH_MAX_SIZE)
    }
    struct EventsByTableName {
        events: Vec<InsertEvent>,
        last_time_to_save: Instant,
    }
    impl Default for EventsByTableName {
        fn default() -> Self {
            Self {
                events: create_batch_size_vec(),
                last_time_to_save: Instant::now(),
            }
        }
    }
    let mut events_map = HashMap::<_, EventsByTableName>::new();
    loop {
        tokio::select! {
            _ = shutdown_signal_rx.recv() => break, // in any case we should correctly finish
            result = data_rx.recv() => {
                match result {
                    Some((event, table_name)) => {
                        let EventsByTableName{ ref mut events, ref mut last_time_to_save } = events_map.entry(table_name).or_default();
                        events.push(event);

                        if last_time_to_save.elapsed() < SAVE_TIMEOUT ||
                            events.len() >= BATCH_SIZE_TO_SAVE {

                            let events = mem::replace(events, create_batch_size_vec());
                            save_batch(&mut client, table_name, events).await.context("from `start_db_event_recorder` in `save_batch`")?;

                            *last_time_to_save = Instant::now();
                        }
                    },
                    None => break, // in any case we should correctly finish
                }
            },
        }
    }

    let _ = shutdown_tx.send(Ok(()));

    Ok(())
}

async fn save_batch(
    client: &mut Client,
    table_name: TableName,
    events: Vec<InsertEvent>,
) -> Result<()> {
    match save_events_batch(client, table_name, &events).await {
        Ok(()) => return Ok(()),
        Err(err) => log::error!("Failed to save batch of events with error: {err:?}"),
    }

    let (saving_result, failed_events) = save_events_one_by_one(client, table_name, events).await;
    match saving_result {
        Ok(()) => {
            if !failed_events.is_empty() {
                save_to_file_fallback(failed_events, table_name);
            }
        }
        Err(err) => {
            log::error!("Failed to save events one by one with error: {err:?}");
            save_to_file_fallback(failed_events, table_name)
        }
    }

    Ok(())
}

fn save_to_file_fallback(_failed_events: Vec<InsertEvent>, _table_name: TableName) {
    // TODO implement fallback with saving failed events in file
}

#[cfg(test)]
mod tests {
    use crate::database::events::EventRecorder;
    use mmb_database::postgres_db::events::{Event, TableName};
    use serde_json::Value;
    use std::time::Duration;
    use tokio::time::sleep;

    use crate::infrastructure::init_lifetime_manager;
    use serde::{Deserialize, Serialize};
    use tokio_postgres::NoTls;

    const DATABASE_URL: &'static str = "postgres://dev:dev@localhost/tests";
    pub const TABLE_NAME: &str = "persons";

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Address {
        street_address: String,
        city: String,
        postal_code: u32,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Person {
        first_name: String,
        last_name: String,
        address: Address,
        phone_numbers: Vec<String>,
    }

    impl Event for Person {
        fn get_table_name(&self) -> TableName {
            TABLE_NAME
        }

        fn get_json(&self) -> serde_json::Result<Value> {
            serde_json::to_value(self)
        }
    }

    fn test_person() -> Person {
        Person {
            first_name: "Иван".to_string(),
            last_name: "Иванов".to_string(),
            address: Address {
                street_address: "Московское ш., 101, кв.101".to_string(),
                city: "Ленинград".to_string(),
                postal_code: 101101,
            },
            phone_numbers: vec!["812 123-1234".to_string(), "916 123-4567".to_string()],
        }
    }

    #[ignore = "need postgres initialized for tests"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_1_event() {
        init_lifetime_manager();

        let (client, connection) = tokio_postgres::connect(DATABASE_URL, NoTls)
            .await
            .expect("connect to DB in test");

        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("connection error: {}", e);
            }
        });

        let _ = client
            .execute(&format!("truncate table {TABLE_NAME}"), &[])
            .await
            .expect("truncate persons");

        let event_recorder = EventRecorder::start(Some(DATABASE_URL.to_string()));

        let person = test_person();
        event_recorder.save(person).expect("in test");

        sleep(Duration::from_secs(2)).await;

        let rows = client
            .query("select * from persons", &[])
            .await
            .expect("select persons in test");

        assert_eq!(rows.len(), 1);
    }

    #[ignore = "need postgres initialized for tests"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn not_save_1_event_without_db_initialization() {
        // arrange
        init_lifetime_manager();

        let (client, connection) = tokio_postgres::connect(DATABASE_URL, NoTls)
            .await
            .expect("connect to DB in test");

        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("connection error: {}", e);
            }
        });

        let _ = client
            .execute(&format!("truncate table {TABLE_NAME}"), &[])
            .await
            .expect("truncate persons");

        let person = test_person();

        let database_url = None; // database_url is not initialized

        // act
        let event_recorder = EventRecorder::start(database_url);

        event_recorder.save(person).expect("in test");

        sleep(Duration::from_secs(2)).await;

        // assert
        let rows = client
            .query("select * from persons", &[])
            .await
            .expect("select persons in test");

        assert_eq!(rows.len(), 0);
    }
}