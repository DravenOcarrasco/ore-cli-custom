use std::{sync::{Arc, atomic::{AtomicBool, Ordering}}};

use colored::*;
use drillx::{
    equix::{self},
    Hash, Solution,
};
use ore_api::{
    consts::{BUS_ADDRESSES, BUS_COUNT, EPOCH_DURATION},
    state::{Bus, Config, Proof},
};
use ore_utils::AccountDeserialize;
use rand::Rng;
use solana_program::pubkey::Pubkey;
use solana_rpc_client::spinner;
use solana_sdk::signer::Signer;

use crate::{
    args::MineArgs,
    send_and_confirm::ComputeBudget,
    utils::{
        amount_u64_to_string, get_clock, get_config, get_updated_proof_with_authority, proof_pubkey,
    },
    Miner,
};

impl Miner {
    pub async fn mine(&self, args: MineArgs) {
        // Open account, if needed.
        let signer = self.signer();
        self.open().await;

        // Check num threads
        self.check_num_cores(args.cores);

        // Start mining loop
        let mut last_hash_at = 0;
        loop {
            // Fetch proof
            let config = get_config(&self.rpc_client).await;
            let proof =
                get_updated_proof_with_authority(&self.rpc_client, signer.pubkey(), last_hash_at)
                    .await;
            last_hash_at = proof.last_hash_at;
            println!(
                "\nStake: {} ORE\n  Multiplier: {:12}x",
                amount_u64_to_string(proof.balance),
                calculate_multiplier(proof.balance, config.top_balance)
            );

            // Calc cutoff time
            let cutoff_time = self.get_cutoff(proof, args.buffer_time).await;

            // Run drillx
            let solution =
                Self::find_hash_par(proof, /*cutoff_time 0, */args.cores, args.min_difficulty)
                    .await;

            // Build instruction set
            let mut ixs = vec![ore_api::instruction::auth(proof_pubkey(signer.pubkey()))];
            let mut compute_budget = 500_000;
            if self.should_reset(config).await && rand::thread_rng().gen_range(0..100).eq(&0) {
                compute_budget += 100_000;
                ixs.push(ore_api::instruction::reset(signer.pubkey()));
            }

            // Build mine ix
            ixs.push(ore_api::instruction::mine(
                signer.pubkey(),
                signer.pubkey(),
                self.find_bus().await,
                solution,
            ));

            // Submit transaction
            self.send_and_confirm(&ixs, ComputeBudget::Fixed(compute_budget), false)
                .await
                .ok();
        }
    }

    async fn find_hash_par(
        proof: Proof,
        // cutoff_time: u64,
        cores: u64,
        min_difficulty: u32,
    ) -> Solution {
        // Dispatch job to each thread
        let stop_flag = Arc::new(AtomicBool::new(false));
        let progress_bar = Arc::new(spinner::new_progress_bar());
        progress_bar.set_message("Mining...");
        let core_ids = core_affinity::get_core_ids().unwrap();
        let handles: Vec<_> = core_ids
            .into_iter()
            .map(|i| {
                let proof = proof.clone();
                let progress_bar = progress_bar.clone();
                let stop_flag = stop_flag.clone();
                std::thread::spawn(move || {
                    let mut nonce = u64::MAX.saturating_div(cores).saturating_mul(i.id as u64);
                    let mut best_nonce = nonce;
                    let mut best_difficulty = 0;
                    let mut best_hash = Hash::default();
                    let mut memory = equix::SolverMemory::new();
                    // Return if core should not be used
                    if (i.id as u64).ge(&cores) {
                        return (0, 0, Hash::default());
                    }

                    // Pin to core
                    let _ = core_affinity::set_for_current(i);

                    // Start hashing
                    // let timer = Instant::now();
                    loop {
                        // Verificar se a flag de parada foi acionada
                        if stop_flag.load(Ordering::Relaxed) {
                            break;
                        }
                        // Create hash
                        if let Ok(hx) = drillx::hash_with_memory(
                            &mut memory,
                            &proof.challenge,
                            &nonce.to_le_bytes(),
                        ) {
                            let difficulty = hx.difficulty();
                            if difficulty.gt(&best_difficulty) {
                                best_nonce = nonce;
                                best_difficulty = difficulty;
                                best_hash = hx;
                            }
                            
                            progress_bar.set_message(format!(
                                "MIN_DIFFICULTY: {} > {} Mining...",
                                min_difficulty,
                                best_difficulty
                            ));

                            // Exit loop if difficulty meets or exceeds min_difficulty
                            if best_difficulty.ge(&min_difficulty) {
                                stop_flag.store(true, Ordering::Relaxed);
                                break;
                            }
                        }
                        nonce += 1;
                    }
                    // Return the best nonce
                    (best_nonce, best_difficulty, best_hash)
                })
            })
            .collect();

        // Join handles and return best nonce
        let mut best_nonce = 0;
        let mut best_difficulty = 0;
        let mut best_hash = Hash::default();
        for h in handles {
            if let Ok((nonce, difficulty, hash)) = h.join() {
                if difficulty > best_difficulty {
                    best_difficulty = difficulty;
                    best_nonce = nonce;
                    best_hash = hash;
                }
            }
        }

        // Update log
        progress_bar.finish_with_message(format!(
            "Best hash: {} (difficulty: {})",
            bs58::encode(best_hash.h).into_string(),
            best_difficulty
        ));

        Solution::new(best_hash.d, best_nonce.to_le_bytes())
    }

    pub fn check_num_cores(&self, cores: u64) {
        let num_cores = num_cpus::get() as u64;
        if cores.gt(&num_cores) {
            println!(
                "{} Cannot exceeds available cores ({})",
                "WARNING".bold().yellow(),
                num_cores
            );
        }
    }

    async fn should_reset(&self, config: Config) -> bool {
        let clock = get_clock(&self.rpc_client).await;
        config
            .last_reset_at
            .saturating_add(EPOCH_DURATION)
            .saturating_sub(5) // Buffer
            .le(&clock.unix_timestamp)
    }

    async fn get_cutoff(&self, proof: Proof, buffer_time: u64) -> u64 {
        let clock = get_clock(&self.rpc_client).await;
        proof
            .last_hash_at
            .saturating_add(60)
            .saturating_sub(buffer_time as i64)
            .saturating_sub(clock.unix_timestamp)
            .max(0) as u64
    }
}

fn calculate_multiplier(balance: u64, top_balance: u64) -> f64 {
    1.0 + (balance as f64 / top_balance as f64).min(1.0f64)
}
