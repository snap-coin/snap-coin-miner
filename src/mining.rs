use anyhow::anyhow;
use core_affinity::CoreId;
use rand::random;
use snap_coin::{
    core::{block::Block, difficulty::calculate_block_difficulty, transaction::TransactionId},
    crypto::{Hash, address_inclusion_filter::AddressInclusionFilter, merkle_tree::MerkleTree},
    economics::EXPIRATION_TIME,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::sync::{broadcast, mpsc};

use crate::pool::PoolInfo;
use crate::stats::StatEvent;

pub struct MiningThread;

impl MiningThread {
    pub fn spawn(
        thread_id: i32,
        mut job_rx: broadcast::Receiver<Block>,
        submission_tx: mpsc::UnboundedSender<Block>,
        hash_counter: Arc<AtomicU64>,
        global_job_id: Arc<AtomicU64>,
        pool_info: Option<PoolInfo>,
        is_pool: bool,
        stat_tx: mpsc::UnboundedSender<StatEvent>,
        mut shutdown: broadcast::Receiver<()>,
        cpu_core: Option<CoreId>,
    ) {
        thread::spawn(move || {
            let mut local_job_id = 0;
            let mut current_block: Option<Block> = None;
            let mut thread_hashes = 0u64;
            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let shutdown_flag_clone = shutdown_flag.clone();

            core_affinity::set_for_current(cpu_core.expect("No core affinity found!"));

            let stat_tx_clone = stat_tx.clone();
            thread::spawn(move || {
                let _ = shutdown.blocking_recv();
                stat_tx_clone
                    .send(StatEvent::Event(format!(
                        "Requested thread shutdown {thread_id}"
                    )))
                    .ok();
                shutdown_flag_clone.store(true, Ordering::Relaxed);
            });

            loop {
                // Fast shutdown check - just atomic load, no syscalls
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }

                if let Err(_) = Self::mine_iteration(
                    thread_id,
                    &mut job_rx,
                    &submission_tx,
                    &hash_counter,
                    &global_job_id,
                    &mut local_job_id,
                    &mut current_block,
                    pool_info,
                    is_pool,
                    &stat_tx,
                    &mut thread_hashes,
                    &shutdown_flag,
                ) {
                    break;
                }
            }
        });
    }

    fn mine_iteration(
        thread_id: i32,
        job_rx: &mut broadcast::Receiver<Block>,
        submission_tx: &mpsc::UnboundedSender<Block>,
        hash_counter: &Arc<AtomicU64>,
        global_job_id: &Arc<AtomicU64>,
        local_job_id: &mut u64,
        current_block: &mut Option<Block>,
        pool_info: Option<PoolInfo>,
        is_pool: bool,
        stat_tx: &mpsc::UnboundedSender<StatEvent>,
        thread_hashes: &mut u64,
        shutdown_flag: &Arc<AtomicBool>,
    ) -> Result<(), anyhow::Error> {
        let current_global_job = global_job_id.load(Ordering::Relaxed);

        // Wait for a new job if we don't have one or are behind
        while current_block.is_none() || current_global_job > *local_job_id {
            if shutdown_flag.load(Ordering::Relaxed) {
                return Err(anyhow!("Requested thread shutdown {thread_id}"));
            }

            match job_rx.try_recv() {
                Ok(job) => {
                    *local_job_id += 1;
                    *current_block = Some(job);
                    current_block.as_mut().unwrap().nonce = random();
                    break;
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    stat_tx
                        .send(StatEvent::Event(format!(
                            "Thread {} lagged by {} jobs",
                            thread_id + 1,
                            skipped
                        )))
                        .ok();
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(anyhow!("Job channel closed"));
                }
            }
        }

        // Only mine if synchronized with global job ID
        if global_job_id.load(Ordering::Relaxed) != *local_job_id {
            thread::sleep(Duration::from_millis(10));
            return Ok(());
        }

        let block = current_block.as_mut().unwrap();
        block.timestamp = chrono::Utc::now().timestamp() as u64;

        // Remove expired transactions with 10s margin
        let mut removed_txs = false;
        block.transactions.retain(|tx| {
            let expired =
                tx.timestamp + EXPIRATION_TIME + 10 < chrono::Utc::now().timestamp() as u64;
            if expired {
                removed_txs = true;
            }
            !expired
        });

        // Update merkle tree and filter if transactions were removed
        if removed_txs {
            block.meta.merkle_tree_root = MerkleTree::build(
                &block
                    .transactions
                    .iter()
                    .map(|tx| tx.transaction_id.unwrap())
                    .collect::<Vec<TransactionId>>(),
            )
            .root_hash();
            block.meta.address_inclusion_filter =
                AddressInclusionFilter::create_filter(&block.transactions)?;
        }

        // Try a new nonce and hash the block
        block.nonce += 1;
        block.meta.hash = Some(Hash::new(&block.get_hashing_buf()?));

        // Increment hash counter and thread-specific counter
        hash_counter.fetch_add(1, Ordering::Relaxed);
        *thread_hashes += 1;

        // Report thread stats every 100 hashes
        if *thread_hashes % 100 == 0 {
            stat_tx
                .send(StatEvent::ThreadHash(thread_id as usize, *thread_hashes))
                .ok();
            *thread_hashes = 0; // Reset
        }

        // Check if the hash meets the difficulty target
        if is_pool {
            if pool_info.unwrap().pool_difficulty > *block.meta.hash.unwrap()
                && global_job_id.load(Ordering::Relaxed) == *local_job_id
            {
                stat_tx
                    .send(StatEvent::Event(format!(
                        "Thread {} found share: {}",
                        thread_id + 1,
                        block.meta.hash.unwrap().dump_base36()
                    )))
                    .ok();
                submission_tx
                    .send(block.clone())
                    .map_err(|e| anyhow!("Failed to send submission: {}", e))?;
            }
        } else {
            if calculate_block_difficulty(
                &block.meta.block_pow_difficulty,
                block.transactions.len(),
            ) > *block.meta.hash.unwrap()
                && global_job_id.load(Ordering::Relaxed) == *local_job_id
            {
                stat_tx
                    .send(StatEvent::Event(format!(
                        "Thread {} found block: {}",
                        thread_id + 1,
                        block.meta.hash.unwrap().dump_base36()
                    )))
                    .ok();
                submission_tx
                    .send(block.clone())
                    .map_err(|e| anyhow!("Failed to send submission: {}", e))?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use snap_coin::{
        core::{
            block::{Block, BlockMetadata},
            difficulty::calculate_block_difficulty,
        },
        crypto::{Hash, address_inclusion_filter::AddressInclusionFilter, merkle_tree::MerkleTree},
    };

    fn empty_block(nonce: u64, timestamp: u64, pow_difficulty: [u8; 32]) -> Block {
        Block {
            transactions: vec![],
            timestamp,
            nonce,
            meta: BlockMetadata {
                block_pow_difficulty: pow_difficulty,
                tx_pow_difficulty: [0xff; 32],
                previous_block: Hash::new_from_buf([0xab; 32]),
                hash: None,
                merkle_tree_root: MerkleTree::build(&[]).root_hash(),
                address_inclusion_filter: AddressInclusionFilter::create_filter(&[]).unwrap(),
            },
        }
    }

    fn compute_hash(block: &mut Block) -> Hash {
        let buf = block.get_hashing_buf().unwrap();
        let h = Hash::new(&buf);
        block.meta.hash = Some(h);
        h
    }

    /// Returns true if hash satisfies difficulty (hash numerically <= target).
    fn satisfies_difficulty(target: Hash, hash: Hash) -> bool {
        target.dump_buf() > hash.dump_buf()
    }

    // ── nonce / hash sanity ───────────────────────────────────────────────────

    #[test]
    fn different_nonces_produce_different_hashes() {
        let mut b1 = empty_block(1_000, 1_750_000_000, [0xff; 32]);
        let mut b2 = empty_block(1_001, 1_750_000_000, [0xff; 32]);
        assert_ne!(
            compute_hash(&mut b1).dump_buf(),
            compute_hash(&mut b2).dump_buf()
        );
    }

    #[test]
    fn hashing_is_deterministic() {
        let mut block = empty_block(42, 1_750_000_000, [0xff; 32]);
        let h1 = compute_hash(&mut block);
        block.meta.hash = None;
        let h2 = compute_hash(&mut block);
        assert_eq!(h1.dump_buf(), h2.dump_buf());
    }

    // ── known-answer tests ────────────────────────────────────────────────────

    #[test]
    fn known_hash_nonce_100_ts_1750000000() {
        let mut block = empty_block(100, 1_750_000_000, [0xff; 32]);
        let hash = compute_hash(&mut block);

        let expected: [u8; 32] = [
            34, 50, 13, 159, 125, 143, 41, 165, 139, 200, 26, 14, 239, 200, 22, 130, 5, 155, 34,
            175, 143, 245, 137, 207, 134, 215, 197, 112, 75, 246, 68, 14,
        ];
        assert_eq!(hash.dump_buf(), expected, "hash mismatch for nonce=100");
    }

    #[test]
    fn known_hash_nonce_999999_ts_1750000000() {
        let mut block = empty_block(999_999, 1_750_000_000, [0xff; 32]);
        let hash = compute_hash(&mut block);

        let expected: [u8; 32] = [
            37, 34, 231, 61, 235, 126, 128, 12, 46, 187, 102, 183, 18, 90, 129, 56, 7, 152, 114,
            170, 166, 127, 101, 112, 117, 59, 87, 131, 204, 92, 119, 218,
        ];
        assert_eq!(hash.dump_buf(), expected, "hash mismatch for nonce=999999");
    }

    // ── validate_block_hash round-trip ────────────────────────────────────────

    #[test]
    fn validate_block_hash_accepts_correct_hash() {
        let mut block = empty_block(1, 1_750_000_000, [0xff; 32]);
        compute_hash(&mut block);
        assert!(block.validate_block_hash().is_ok());
    }

    #[test]
    fn validate_block_hash_rejects_tampered_hash() {
        let mut block = empty_block(1, 1_750_000_000, [0xff; 32]);
        compute_hash(&mut block);

        let mut raw = block.meta.hash.unwrap().dump_buf();
        raw[0] ^= 0xff;
        block.meta.hash = Some(Hash::new_from_buf(raw));

        assert!(block.validate_block_hash().is_err());
    }

    // ── difficulty comparisons ────────────────────────────────────────────────

    #[test]
    fn max_difficulty_never_satisfied_for_first_100_nonces() {
        let pow = [0x00; 32]; // all-zero target: nothing can be <= this
        let mut block = empty_block(0, 1_750_000_000, pow);
        let target = calculate_block_difficulty(&block.meta.block_pow_difficulty, 0);

        for nonce in 0u64..100 {
            block.nonce = nonce;
            block.meta.hash = None;
            let hash = compute_hash(&mut block);
            assert!(
                !satisfies_difficulty(Hash::new_from_buf(target), hash),
                "nonce {nonce} unexpectedly satisfied an impossible difficulty"
            );
        }
    }

    #[test]
    fn min_difficulty_always_satisfied() {
        let pow = [0xff; 32]; // all-ones target: every hash passes
        let mut block = empty_block(0, 1_750_000_000, pow);
        let target = calculate_block_difficulty(&block.meta.block_pow_difficulty, 0);
        let hash = compute_hash(&mut block);

        assert!(
            satisfies_difficulty(Hash::new_from_buf(target), hash),
            "expected any hash to satisfy all-0xff difficulty"
        );
    }

    // ── merkle + filter stability ─────────────────────────────────────────────

    #[test]
    fn empty_block_merkle_and_filter_are_stable() {
        let block = empty_block(7, 1_750_000_000, [0x80; 32]);
        assert!(block.validate_merkle_tree().is_ok());
        assert!(block.validate_address_inclusion_filter().is_ok());
    }

    // ── validate_difficulties ─────────────────────────────────────────────────

    #[test]
    fn validate_difficulties_accepts_matching_difficulties() {
        let pow = [0xff; 32];
        let tx_pow = [0xff; 32];
        let mut block = empty_block(0, 1_750_000_000, pow);
        compute_hash(&mut block);

        assert!(block.validate_difficulties(&pow, &tx_pow).is_ok());
    }

    #[test]
    fn validate_difficulties_rejects_wrong_difficulty() {
        let pow = [0xff; 32];
        let tx_pow = [0xff; 32];
        let mut block = empty_block(0, 1_750_000_000, pow);
        compute_hash(&mut block);

        let wrong_pow = [0x80; 32];
        assert!(block.validate_difficulties(&wrong_pow, &tx_pow).is_err());
    }
}
