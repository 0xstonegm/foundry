use std::path::PathBuf;

use alloy_primitives::U256;
use alloy_providers::tmp::TempProvider;
use alloy_rpc_types::BlockTransactions;
use cast::{decode::decode_console_logs, revm::primitives::EnvWithHandlerCfg};
use clap::Parser;
use eyre::{Result, WrapErr};
use foundry_cli::{
    init_progress,
    opts::RpcOpts,
    update_progress,
    utils::{handle_traces, TraceResult},
};
use foundry_common::{is_known_system_sender, SYSTEM_TRANSACTION_TYPE};
use foundry_compilers::EvmVersion;
use foundry_config::{find_project_root_path, Config};
use foundry_evm::{
    executors::{EvmError, TracingExecutor},
    opts::EvmOpts,
    utils::configure_tx_env,
};
use foundry_tweak::tweak_backend;

/// CLI arguments for `cast run`.
#[derive(Clone, Debug, Parser)]
pub struct RunArgs {
    /// The transaction hash.
    tx_hash: String,

    /// Opens the transaction in the debugger.
    #[arg(long, short)]
    debug: bool,

    /// Print out opcode traces.
    #[deprecated]
    #[arg(long, short, hide = true)]
    trace_printer: bool,

    /// Executes the transaction only with the state from the previous block.
    /// Note that this also include transactions that are used for tweaking code.
    ///
    /// May result in different results than the live execution!
    #[arg(long, short)]
    quick: bool,

    /// Prints the full address of the contract.
    #[arg(long, short)]
    verbose: bool,

    /// Label addresses in the trace.
    ///
    /// Example: 0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045:vitalik.eth
    #[arg(long, short)]
    label: Vec<String>,

    #[command(flatten)]
    rpc: RpcOpts,

    /// The EVM version to use.
    ///
    /// Overrides the version specified in the config.
    #[arg(long, short)]
    evm_version: Option<EvmVersion>,

    /// Sets the number of assumed available compute units per second for this provider
    ///
    /// default value: 330
    ///
    /// See also, https://docs.alchemy.com/reference/compute-units#what-are-cups-compute-units-per-second
    #[arg(long, alias = "cups", value_name = "CUPS")]
    pub compute_units_per_second: Option<u64>,

    /// Disables rate limiting for this node's provider.
    ///
    /// default value: false
    ///
    /// See also, https://docs.alchemy.com/reference/compute-units#what-are-cups-compute-units-per-second
    #[arg(long, value_name = "NO_RATE_LIMITS", visible_alias = "no-rpc-rate-limit")]
    pub no_rate_limit: bool,

    /// One `forge clone`d project that will be used to tweak the code of the corresponding
    /// on-chain contract.
    ///
    /// This option can be used multiple times to tweak multiple contracts.
    #[arg(long, value_name = "CLONED_PROJECT")]
    pub tweak: Vec<PathBuf>,
}

impl RunArgs {
    /// Executes the transaction by replaying it
    ///
    /// This replays the entire block the transaction was mined in unless `quick` is set to true
    ///
    /// Note: This executes the transaction(s) as is: Cheatcodes are disabled
    pub async fn run(self) -> Result<()> {
        #[allow(deprecated)]
        if self.trace_printer {
            eprintln!("WARNING: --trace-printer is deprecated and has no effect\n");
        }

        let figment = Config::figment_with_root(find_project_root_path(None).unwrap())
            .merge(self.rpc.clone());
        let evm_opts = figment.extract::<EvmOpts>()?;
        let mut config = Config::try_from(figment)?.sanitized();

        let compute_units_per_second =
            if self.no_rate_limit { Some(u64::MAX) } else { self.compute_units_per_second };

        let provider = foundry_common::provider::alloy::ProviderBuilder::new(
            &config.get_rpc_url_or_localhost_http()?,
        )
        .compute_units_per_second_opt(compute_units_per_second)
        .build()?;

        let tx_hash = self.tx_hash.parse().wrap_err("invalid tx hash")?;
        let tx = provider
            .get_transaction_by_hash(tx_hash)
            .await
            .wrap_err_with(|| format!("tx not found: {:?}", tx_hash))?;

        // check if the tx is a system transaction
        if is_known_system_sender(tx.from) ||
            tx.transaction_type.map(|ty| ty.to::<u64>()) == Some(SYSTEM_TRANSACTION_TYPE)
        {
            return Err(eyre::eyre!(
                "{:?} is a system transaction.\nReplaying system transactions is currently not supported.",
                tx.hash
            ));
        }

        let tx_block_number = tx
            .block_number
            .ok_or_else(|| eyre::eyre!("tx may still be pending: {:?}", tx_hash))?
            .to::<u64>();

        // fetch the block the transaction was mined in
        let block = provider.get_block(tx_block_number.into(), true).await?;

        // we need to fork off the parent block
        config.fork_block_number = Some(tx_block_number - 1);

        let (mut env, fork, chain) = TracingExecutor::get_fork_material(&config, evm_opts).await?;

        let mut evm_version = self.evm_version;

        env.block.number = U256::from(tx_block_number);

        if let Some(block) = &block {
            env.block.timestamp = block.header.timestamp;
            env.block.coinbase = block.header.miner;
            env.block.difficulty = block.header.difficulty;
            env.block.prevrandao = Some(block.header.mix_hash.unwrap_or_default());
            env.block.basefee = block.header.base_fee_per_gas.unwrap_or_default();
            env.block.gas_limit = block.header.gas_limit;

            // TODO: we need a smarter way to map the block to the corresponding evm_version for
            // commonly used chains
            if evm_version.is_none() {
                // if the block has the excess_blob_gas field, we assume it's a Cancun block
                if block.header.excess_blob_gas.is_some() {
                    evm_version = Some(EvmVersion::Cancun);
                }
            }
        }

        let mut executor = TracingExecutor::new(env.clone(), fork, evm_version, self.debug);
        if !self.tweak.is_empty() {
            // If user specified tweak projects, we need to tweak the code of the contracts
            let mut cloned_projects: Vec<foundry_tweak::ClonedProject> = vec![];
            for path in self.tweak.iter() {
                let path = dunce::canonicalize(path)
                    .map_err(|e| eyre::eyre!("failed to load tweak project: {:?}", e))?;
                let project =
                    foundry_tweak::ClonedProject::load_with_root(&path).wrap_err_with(|| {
                        format!("failed to load tweak project from path: {:?}", &path)
                    })?;
                cloned_projects.push(project);
            }
            let tweak_map =
                foundry_tweak::build_tweak_data(&cloned_projects, &self.rpc, self.quick).await?;
            tweak_backend(&mut executor.backend, &tweak_map)?;
        }
        println!("Executing transaction: {:?}", tx.hash);

        let mut env =
            EnvWithHandlerCfg::new_with_spec_id(Box::new(env.clone()), executor.spec_id());

        // Set the state to the moment right before the transaction
        if !self.quick {
            println!("Executing previous transactions from the block.");

            if let Some(block) = block {
                let pb = init_progress!(block.transactions, "tx");
                pb.set_position(0);

                let BlockTransactions::Full(txs) = block.transactions else {
                    return Err(eyre::eyre!("Could not get block txs"))
                };

                for (index, tx) in txs.into_iter().enumerate() {
                    // System transactions such as on L2s don't contain any pricing info so
                    // we skip them otherwise this would cause
                    // reverts
                    if is_known_system_sender(tx.from) ||
                        tx.transaction_type.map(|ty| ty.to::<u64>()) ==
                            Some(SYSTEM_TRANSACTION_TYPE)
                    {
                        update_progress!(pb, index);
                        continue;
                    }
                    if tx.hash == tx_hash {
                        break;
                    }

                    configure_tx_env(&mut env, &tx);

                    if let Some(to) = tx.to {
                        trace!(tx=?tx.hash,?to, "executing previous call transaction");
                        executor.commit_tx_with_env(env.clone()).wrap_err_with(|| {
                            format!(
                                "Failed to execute transaction: {:?} in block {}",
                                tx.hash, env.block.number
                            )
                        })?;
                    } else {
                        trace!(tx=?tx.hash, "executing previous create transaction");
                        if let Err(error) = executor.deploy_with_env(env.clone(), None) {
                            match error {
                                // Reverted transactions should be skipped
                                EvmError::Execution(_) => (),
                                error => {
                                    return Err(error).wrap_err_with(|| {
                                        format!(
                                            "Failed to deploy transaction: {:?} in block {}",
                                            tx.hash, env.block.number
                                        )
                                    })
                                }
                            }
                        }
                    }

                    update_progress!(pb, index);
                }
            }
        }

        // Execute our transaction
        let (result, console_logs) = {
            configure_tx_env(&mut env, &tx);

            if let Some(to) = tx.to {
                trace!(tx=?tx.hash, to=?to, "executing call transaction");
                let result = executor.commit_tx_with_env(env)?;
                let logs = decode_console_logs(&result.logs);
                (TraceResult::from(result), logs)
            } else {
                trace!(tx=?tx.hash, "executing create transaction");
                match executor.deploy_with_env(env, None) {
                    Ok(res) => {
                        let logs = decode_console_logs(&res.logs);
                        (TraceResult::from(res), logs)
                    }
                    Err(err) => {
                        let logs = match &err {
                            EvmError::Execution(e) => decode_console_logs(&e.logs),
                            _ => vec![],
                        };
                        (TraceResult::try_from(err)?, logs)
                    }
                }
            }
        };

        handle_traces(result, &config, chain, self.label, self.debug).await?;

        // print logs if any
        if !console_logs.is_empty() {
            println!("Logs:");
            for log in console_logs {
                println!("  {log}");
            }
            println!();
        }

        Ok(())
    }
}
