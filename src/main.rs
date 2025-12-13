use anyhow::anyhow;
use argon2::{Argon2, Params};
use config::Config;
use num_bigint::BigUint;
use rand::Rng;
use snap_coin::{
    UtilError,
    api::client::Client,
    blockchain_data_provider::BlockchainDataProvider,
    build_block,
    core::{block::Block, difficulty::calculate_block_difficulty},
    crypto::{ARGON2_CONFIG, Hash, keys::Public},
    economics::GENESIS_PREVIOUS_BLOCK_HASH,
};
use std::{
    env::args,
    fs::{self, File},
    io::Write,
    sync::{Arc, RwLock},
    thread,
    time::Duration,
};

const BATCH_SIZE: u64 = 20;

type Difficulty = Arc<RwLock<BigUint>>;
type BlockRef = Arc<RwLock<Block>>;

fn mine_thread(
    client: Arc<Client>,
    block_ref: BlockRef,
    difficulty: Difficulty,
    thread_id: usize,
    hashes_counter: Arc<RwLock<u64>>,
    last_block_time: Arc<RwLock<i64>>,
) {
    let params = Params::new(
        ARGON2_CONFIG.memory_cost,
        ARGON2_CONFIG.time_cost,
        ARGON2_CONFIG.parallelism,
        ARGON2_CONFIG.output_length,
    )
    .expect("Failed to create Argon2 params");
    let argon2 = Argon2::new(ARGON2_CONFIG.algorithm, ARGON2_CONFIG.version, params);
    let mut hash_buf = [0xFFu8; ARGON2_CONFIG.output_length.unwrap()];

    let mut make_hash = |buf: &[u8]| -> Result<BigUint, argon2::Error> {
        argon2.hash_password_into(buf, &ARGON2_CONFIG.magic_bytes, &mut hash_buf)?;
        Ok(BigUint::from_bytes_be(&hash_buf))
    };

    let mut rng = rand::rng();

    loop {
        // grab current block and difficulty
        let local_block = { block_ref.read().unwrap().clone() };
        let local_difficulty = { difficulty.read().unwrap().clone() };

        for _ in 0..BATCH_SIZE {
            let mut trial_block = local_block.clone();
            trial_block.nonce = rng.random::<u64>();

            let buf = match trial_block.get_hashing_buf() {
                Ok(b) => b,
                Err(_) => continue,
            };

            let trial_hash = match make_hash(&buf) {
                Ok(h) => h,
                Err(_) => continue,
            };

            if trial_hash <= local_difficulty {
                trial_block.hash = Some(Hash::new(&buf));

                // Recheck difficulty before submit
                let submit_difficulty = { difficulty.read().unwrap().clone() };
                if trial_hash <= submit_difficulty {
                    match futures::executor::block_on(client.submit_block(trial_block.clone())) {
                        Err(e) => println!("[Thread {}] Block submit failed: {}", thread_id, e),
                        Ok(blockchain_result) => {
                            if let Ok(()) = blockchain_result {
                                let now = chrono::Utc::now().timestamp();
                                println!(
                                    "[Thread {}] Block submitted: {}, took: {}s",
                                    thread_id,
                                    trial_block.hash.unwrap().dump_base36(),
                                    now - *last_block_time.read().unwrap()
                                );
                                *last_block_time.write().unwrap() = now;
                            }
                        }
                    }
                }
                break;
            }
        }

        // Update hashes counter
        {
            let mut h = hashes_counter.write().unwrap();
            *h += BATCH_SIZE;
        }

        // Yield a tiny bit
        thread::sleep(Duration::from_millis(1));
    }
}

async fn get_current_block(client: &Client, miner: Public) -> Result<Block, UtilError> {
    let mempool = client.get_mempool().await?;
    build_block(client, &mempool, miner).await
}

fn start_block_refresh(
    client: Arc<Client>,
    miner: Public,
    block_ref: BlockRef,
    difficulty: Difficulty,
) {
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        loop {
            if let Ok(new_block) = rt.block_on(get_current_block(&client, miner)) {
                {
                    let mut b = block_ref.write().unwrap();
                    *b = new_block.clone();
                }

                if let Ok(node_diff) = rt.block_on(client.get_block_difficulty()) {
                    let calculated = BigUint::from_bytes_be(&calculate_block_difficulty(
                        &node_diff,
                        new_block.transactions.len(),
                    ));
                    let mut d = difficulty.write().unwrap();
                    *d = calculated;
                }
            }
            thread::sleep(Duration::from_secs(3));
        }
    });
}

fn start_stats_thread(hashes_counter: Arc<RwLock<u64>>) {
    thread::spawn(move || {
        const INTERVAL: u64 = 3;
        loop {
            thread::sleep(Duration::from_secs(INTERVAL));
            let mut h = hashes_counter.write().unwrap();
            let hs = *h;
            *h = 0;
            println!("Hashes per second: {:.2} H/s", hs as f64 / INTERVAL as f64);
        }
    });
}

const DEFAULT_CONFIG: &str = "[node]
address = \"127.0.0.1:3003\"

[miner]
public = \"<your public wallet address>\"

[threads]
count = 1";

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut config_path = "./miner.toml";

    let args: Vec<String> = args().into_iter().collect();
    for (place, arg) in args.iter().enumerate() {
        if arg == "--config" && args.get(place + 1).is_some() {
            config_path = &args[place + 1];
        }
    }

    if !fs::exists(config_path).is_ok_and(|exists| exists == true) {
        File::create(config_path)?.write(DEFAULT_CONFIG.as_bytes())?;
        return Err(anyhow!(
            "Created new config file: {}. Please replace <your public wallet address> in the config with your real miner address",
            config_path
        ));
    }

    let settings = Config::builder()
        .add_source(config::File::with_name("miner.toml"))
        .build()?;

    let node_address: String = settings.get("node.address")?;
    let private_key_base36: String = settings.get("miner.public")?;
    let thread_count: i32 = settings.get("threads.count")?;

    let miner_pub = Public::new_from_base36(&private_key_base36).expect("Invalid public key");
    let client = Arc::new(Client::connect(node_address.parse().unwrap()).await?);

    let initial_block =
        Block::new_block_now(vec![], &[0u8; 32], &[0u8; 32], GENESIS_PREVIOUS_BLOCK_HASH);
    let block_ref = Arc::new(RwLock::new(initial_block));
    let difficulty: Difficulty = Arc::new(RwLock::new(BigUint::from(0u32)));
    let hashes_counter = Arc::new(RwLock::new(0u64));
    let last_block_time = Arc::new(RwLock::new(chrono::Utc::now().timestamp()));

    start_block_refresh(
        client.clone(),
        miner_pub.clone(),
        block_ref.clone(),
        difficulty.clone(),
    );
    start_stats_thread(hashes_counter.clone());

    let num_threads = if thread_count == -1 {
        num_cpus::get()
    } else {
        thread_count as usize
    };
    println!("Starting mining with {} threads", num_threads);

    let mut handles = vec![];
    for i in 0..num_threads {
        let client = client.clone();
        let block_ref = block_ref.clone();
        let difficulty = difficulty.clone();
        let hashes_counter = hashes_counter.clone();
        let last_block_time = last_block_time.clone();

        handles.push(thread::spawn(move || {
            mine_thread(
                client,
                block_ref,
                difficulty,
                i,
                hashes_counter,
                last_block_time,
            )
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    Ok(())
}
