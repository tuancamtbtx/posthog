use std::{sync::Arc, time::Duration};

use ahash::AHashSet;
use axum::{routing::get, Router};
use envconfig::Envconfig;
use futures::future::ready;
use property_defs_rs::{
    app_context::AppContext,
    config::Config,
    message_to_event,
    metrics_consts::{
        BATCH_ACQUIRE_TIME, CACHE_CONSUMED, COMPACTED_UPDATES, EVENTS_RECEIVED, FORCED_SMALL_BATCH,
        PERMIT_WAIT_TIME, RECV_DEQUEUED, TRANSACTION_LIMIT_SATURATION, UPDATES_FILTERED_BY_CACHE,
        UPDATES_PER_EVENT, UPDATES_SEEN, UPDATE_ISSUE_TIME, WORKER_BLOCKED,
    },
    types::Update,
};
use quick_cache::sync::Cache;
use rdkafka::{
    consumer::{Consumer, StreamConsumer},
    ClientConfig,
};
use serve_metrics::{serve, setup_metrics_routes};
use tokio::{
    sync::{
        mpsc::{self, error::TrySendError},
        Semaphore,
    },
    task::JoinHandle,
};
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

fn setup_tracing() {
    let log_layer: tracing_subscriber::filter::Filtered<
        tracing_subscriber::fmt::Layer<tracing_subscriber::Registry>,
        EnvFilter,
        tracing_subscriber::Registry,
    > = tracing_subscriber::fmt::layer().with_filter(EnvFilter::from_default_env());
    tracing_subscriber::registry().with(log_layer).init();
}

pub async fn index() -> &'static str {
    "property definitions service"
}

fn start_health_liveness_server(config: &Config, context: Arc<AppContext>) -> JoinHandle<()> {
    let config = config.clone();
    let router = Router::new()
        .route("/", get(index))
        .route("/_readiness", get(index))
        .route(
            "/_liveness",
            get(move || ready(context.liveness.get_status())),
        );
    let router = setup_metrics_routes(router);
    let bind = format!("{}:{}", config.host, config.port);
    tokio::task::spawn(async move {
        serve(router, &bind)
            .await
            .expect("failed to start serving metrics");
    })
}

async fn spawn_producer_loop(
    consumer: Arc<StreamConsumer>,
    channel: mpsc::Sender<Update>,
    shared_cache: Arc<Cache<Update, ()>>,
    skip_threshold: usize,
    compaction_batch_size: usize,
) {
    let mut batch = AHashSet::with_capacity(compaction_batch_size);
    let mut last_send = tokio::time::Instant::now();
    loop {
        let message = consumer
            .recv()
            .await
            .expect("TODO - workers panic on kafka recv fail");

        let Some(event) = message_to_event(message) else {
            continue;
        };

        let updates = event.into_updates(skip_threshold);

        metrics::counter!(EVENTS_RECEIVED).increment(1);
        metrics::counter!(UPDATES_SEEN).increment(updates.len() as u64);
        metrics::histogram!(UPDATES_PER_EVENT).record(updates.len() as f64);

        for update in updates {
            if batch.contains(&update) {
                metrics::counter!(COMPACTED_UPDATES).increment(1);
                continue;
            }
            batch.insert(update);

            if batch.len() >= compaction_batch_size || last_send.elapsed() > Duration::from_secs(10)
            {
                last_send = tokio::time::Instant::now();
                for update in batch.drain() {
                    if shared_cache.get(&update).is_some() {
                        metrics::counter!(UPDATES_FILTERED_BY_CACHE).increment(1);
                        continue;
                    }
                    shared_cache.insert(update.clone(), ());
                    match channel.try_send(update) {
                        Ok(_) => {}
                        Err(TrySendError::Full(update)) => {
                            warn!("Worker blocked");
                            metrics::counter!(WORKER_BLOCKED).increment(1);
                            channel.send(update).await.unwrap();
                        }
                        Err(e) => {
                            warn!("Coordinator send failed: {:?}", e);
                            return;
                        }
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_tracing();
    info!("Starting up...");

    let config = Config::init_from_env()?;

    let kafka_config: ClientConfig = (&config.kafka).into();

    let consumer: Arc<StreamConsumer> = Arc::new(kafka_config.create()?);

    let context = Arc::new(AppContext::new(&config).await?);

    consumer.subscribe(&[config.kafka.event_topic.as_str()])?;

    info!("Subscribed to topic: {}", config.kafka.event_topic);

    start_health_liveness_server(&config, context.clone());

    let (tx, mut rx) = mpsc::channel(config.update_batch_size * config.channel_slots_per_worker);
    let transaction_limit = Arc::new(Semaphore::new(config.max_concurrent_transactions));
    let cache = Arc::new(Cache::new(config.cache_capacity));

    for _ in 0..config.worker_loop_count {
        tokio::spawn(spawn_producer_loop(
            consumer.clone(),
            tx.clone(),
            cache.clone(),
            config.update_count_skip_threshold,
            config.compaction_batch_size,
        ));
    }

    loop {
        let mut batch = Vec::with_capacity(config.update_batch_size);

        let batch_start = tokio::time::Instant::now();
        let batch_time = common_metrics::timing_guard(BATCH_ACQUIRE_TIME, &[]);
        while batch.len() < config.update_batch_size {
            context.worker_liveness.report_healthy().await;

            let remaining_capacity = config.update_batch_size - batch.len();
            // We race these two, so we can escape this loop and do a small batch if we've been waiting too long
            let recv = rx.recv_many(&mut batch, remaining_capacity);
            let sleep = tokio::time::sleep(Duration::from_secs(1));

            tokio::select! {
                got = recv => {
                    if got == 0 {
                        warn!("Coordinator recv failed, dying");
                        return Ok(());
                    }
                    metrics::gauge!(RECV_DEQUEUED).set(got as f64);
                    continue;
                }
                _ = sleep => {
                    if batch_start.elapsed() > Duration::from_secs(config.max_issue_period) {
                        warn!("Forcing small batch due to time limit");
                        metrics::counter!(FORCED_SMALL_BATCH).increment(1);
                        break;
                    }
                }
            }
        }
        batch_time.fin();

        metrics::gauge!(CACHE_CONSUMED).set(cache.len() as f64);

        metrics::gauge!(TRANSACTION_LIMIT_SATURATION).set(
            (config.max_concurrent_transactions - transaction_limit.available_permits()) as f64,
        );

        // We unconditionally wait to acquire a transaction permit - this is our backpressure mechanism. If we
        // fail to acquire a permit for long enough, we will fail liveness checks (but that implies our ongoing
        // transactions are halted, at which point DB health is a concern).
        let permit_acquire_time = common_metrics::timing_guard(PERMIT_WAIT_TIME, &[]);
        let permit = transaction_limit.clone().acquire_owned().await.unwrap();
        permit_acquire_time.fin();

        let context = context.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let issue_time = common_metrics::timing_guard(UPDATE_ISSUE_TIME, &[]);
            context.issue(batch).await.unwrap();
            issue_time.fin();
        });
    }
}
