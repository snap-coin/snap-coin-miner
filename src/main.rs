use anyhow::anyhow;
use config::Config;
use snap_coin::{
    api::client::Client,
    blockchain_data_provider::BlockchainDataProvider,
    crypto::{Hash, keys::Public, randomx_optimized_mode},
};
use std::{
    env::args,
    fs::{self, File},
    io::Write,
    net::{SocketAddr, ToSocketAddrs},
    sync::{Arc, atomic::AtomicU64},
    thread,
    time::Duration,
};
use tokio::sync::{broadcast, mpsc};

mod jobs;
mod mining;
mod pool;
mod stats;
mod tui;
mod utils;

use jobs::JobManager;
use mining::MiningThread;
use stats::{MinerStats, StatEvent};
use tui::TuiManager;

const DEFAULT_CONFIG: &str = "[node]
address = \"<node address or pool address>\"

[miner]
public = \"<your public wallet address>\"

[threads]
count = -1";

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut config_path = "./miner.toml";
    let args: Vec<String> = args().into_iter().collect();
    let mut full_dataset = true;
    let mut huge_pages = true;
    let mut is_pool = false;

    // Parse command line arguments
    for (place, arg) in args.iter().enumerate() {
        if arg == "--config" && args.get(place + 1).is_some() {
            config_path = &args[place + 1];
        }
        if arg == "--no-dataset" {
            full_dataset = false;
        }
        if arg == "--no-huge-pages" {
            huge_pages = false;
        }
        if arg == "--pool" {
            is_pool = true;
        }
    }

    if full_dataset {
        randomx_optimized_mode(huge_pages);
        Hash::new(b"INIT");
    }

    // Create default config if it doesn't exist
    if !fs::exists(config_path).is_ok_and(|exists| exists == true) {
        File::create(config_path)?.write(DEFAULT_CONFIG.as_bytes())?;
        return Err(anyhow!(
            "Created new config file: {}. Please replace <your public wallet address> and <node address or pool address> in the config with your real miner address and pool / node address",
            config_path
        ));
    }

    // Load configuration
    let settings = Config::builder()
        .add_source(config::File::with_name(config_path))
        .build()?;

    let node_address: SocketAddr = settings
        .get::<String>("node.address")?
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "No address found"))?;
    let public_key_base36: String = settings.get("miner.public")?;
    let thread_count: i32 = settings.get("threads.count")?;
    let thread_count = if thread_count == -1 {
        thread::available_parallelism()?.get() as i32
    } else {
        thread_count
    };

    let miner_pub = Public::new_from_base36(&public_key_base36).expect("Invalid public key");

    // Create channels for block submissions and job distribution
    let (stat_tx, stat_rx) = mpsc::unbounded_channel();

    // Start TUI
    let tui_manager = TuiManager::new(thread_count as usize, is_pool);
    tui_manager.start(stat_rx);

    // Start miner with auto-reconnect
    tokio::spawn(async move {
        loop {
            let (shutdown_tx, _) = broadcast::channel::<()>(16);
            let (job_tx, _) = broadcast::channel(64);

            let hash_counter = Arc::new(AtomicU64::new(0));
            let global_job_id = Arc::new(AtomicU64::new(0));

            let result = run_miner(
                node_address,
                miner_pub,
                thread_count,
                is_pool,
                job_tx.clone(),
                hash_counter.clone(),
                global_job_id.clone(),
                stat_tx.clone(),
                shutdown_tx.clone(),
            )
            .await;
            if let Err(e) = result {
                stat_tx
                    .send(StatEvent::Event(format!(
                        "Connection lost: {}. Reconnecting in 15s... ({} subscribers)",
                        e,
                        shutdown_tx.receiver_count()
                    )))
                    .ok();
                shutdown_tx.send(()).ok();
                tokio::time::sleep(Duration::from_secs(15)).await;
                stat_tx
                    .send(StatEvent::Event(format!(
                        "Attempting reconnection... (remaining {} subscribers)",
                        shutdown_tx.receiver_count()
                    )))
                    .ok();
            }
        }
    });

    // Keep main thread alive
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

async fn run_miner(
    node_address: SocketAddr,
    miner_pub: Public,
    thread_count: i32,
    is_pool: bool,
    job_tx: broadcast::Sender<snap_coin::core::block::Block>,
    hash_counter: Arc<AtomicU64>,
    global_job_id: Arc<AtomicU64>,
    stat_tx: mpsc::UnboundedSender<StatEvent>,
    shutdown: broadcast::Sender<()>,
) -> Result<(), anyhow::Error> {
    stat_tx
        .send(StatEvent::Event(format!(
            "Connecting to node at {}...",
            node_address
        )))
        .ok();

    // Connect to node with three separate clients
    let submission_client = Arc::new(Client::connect(node_address).await?);
    let event_client = Client::connect(node_address).await?;
    let job_client = Arc::new(Client::connect(node_address).await?);

    stat_tx
        .send(StatEvent::Event("Connected successfully".to_string()))
        .ok();

    // Initialize pool connection if in pool mode
    let pool_info = if is_pool {
        stat_tx
            .send(StatEvent::Event(
                "Initializing pool connection...".to_string(),
            ))
            .ok();
        let info = pool::initialize_pool_connections(
            &*submission_client,
            &event_client,
            &*job_client,
            miner_pub,
        )
        .await?;
        stat_tx
            .send(StatEvent::Event(format!(
                "Pool connected - Difficulty: {:.2}",
                utils::normalize_difficulty(&info.pool_difficulty)
            )))
            .ok();
        Some(info)
    } else {
        None
    };

    // Create a new submission receiver for this connection
    let (new_submission_tx, submission_rx) = mpsc::unbounded_channel();

    // Start mining threads if not already started
    let current_job_id = global_job_id.load(std::sync::atomic::Ordering::Relaxed);
    let cores = core_affinity::get_core_ids().unwrap();
    if current_job_id == 0 {
        for i in 0..thread_count {
            stat_tx
                .send(StatEvent::Event(format!("Starting thread {}", i + 1)))
                .ok();
            MiningThread::spawn(
                i,
                job_tx.subscribe(),
                new_submission_tx.clone(),
                hash_counter.clone(),
                global_job_id.clone(),
                pool_info,
                is_pool,
                stat_tx.clone(),
                shutdown.subscribe(),
                cores.get(i as usize).cloned(),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Start stats monitor
    let stats_monitor = MinerStats::new(hash_counter.clone(), stat_tx.clone());
    stats_monitor.start(shutdown.subscribe());

    // Start job manager
    let job_manager = JobManager::new(
        job_client,
        job_tx.clone(),
        global_job_id.clone(),
        miner_pub,
        is_pool,
        stat_tx.clone(),
        shutdown.subscribe(),
    );

    // Start submission handler and job manager
    tokio::select! {
        res = handle_submissions(
            submission_rx,
            submission_client,
            is_pool,
            stat_tx.clone(),
        ) => {
            stat_tx.send(StatEvent::Event("Submission manager died!".to_string())).ok();
            res
        },
        res = job_manager.start(event_client) => {
            stat_tx.send(StatEvent::Event("Job manager died!".to_string())).ok();
            res
        },
    }?;

    Ok(())
}

async fn handle_submissions(
    mut submission_rx: mpsc::UnboundedReceiver<snap_coin::core::block::Block>,
    submission_client: Arc<Client>,
    is_pool: bool,
    stat_tx: mpsc::UnboundedSender<StatEvent>,
) -> Result<(), anyhow::Error> {
    loop {
        if let Some(candidate) = submission_rx.recv().await {
            match submission_client.submit_block(candidate).await {
                Ok(Ok(_)) => {
                    if !is_pool {
                        if let Ok(height) = submission_client.get_height().await {
                            let block_reward =
                                snap_coin::economics::get_block_reward(height.saturating_sub(1));
                            let net_reward = snap_coin::to_snap(
                                block_reward
                                    - snap_coin::economics::calculate_dev_fee(block_reward),
                            );
                            stat_tx
                                .send(StatEvent::Event(format!(
                                    "Block accepted! Reward: {} SNAP",
                                    net_reward
                                )))
                                .ok();
                            stat_tx.send(StatEvent::BlockAccepted).ok();
                        }
                    } else {
                        stat_tx
                            .send(StatEvent::Event("Share accepted".to_string()))
                            .ok();
                        stat_tx.send(StatEvent::ShareAccepted).ok();
                    }
                }
                Ok(Err(e)) => {
                    if !is_pool {
                        stat_tx
                            .send(StatEvent::Event(format!("Block rejected: {}", e)))
                            .ok();
                        stat_tx.send(StatEvent::BlockRejected).ok();
                    } else {
                        stat_tx
                            .send(StatEvent::Event(format!("Share rejected: {}", e)))
                            .ok();
                        stat_tx.send(StatEvent::ShareRejected).ok();
                    }
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
    }
}
