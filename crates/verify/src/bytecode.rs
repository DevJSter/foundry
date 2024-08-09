//! The `forge verify-bytecode` command.
use crate::{
    etherscan::EtherscanVerificationProvider,
    utils::{BytecodeType, JsonResult},
    verify::VerifierArgs,
};
use alloy_dyn_abi::DynSolValue;
use alloy_primitives::{hex, Address, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types::{BlockId, BlockNumberOrTag, Transaction};
use clap::{Parser, ValueHint};
use eyre::{OptionExt, Result};
use foundry_cli::{
    opts::EtherscanOpts,
    utils::{self, read_constructor_args_file, LoadConfig},
};
use foundry_common::abi::encode_args;
use foundry_compilers::{artifacts::EvmVersion, info::ContractInfo};
use foundry_config::{figment, impl_figment_convert, Config};
use foundry_evm::{
    constants::DEFAULT_CREATE2_DEPLOYER, executors::TracingExecutor, utils::configure_tx_env,
};
use revm_primitives::{db::Database, AccountInfo, EnvWithHandlerCfg, HandlerCfg};
use std::path::PathBuf;
use yansi::Paint;

impl_figment_convert!(VerifyBytecodeArgs);

/// CLI arguments for `forge verify-bytecode`.
#[derive(Clone, Debug, Parser)]
pub struct VerifyBytecodeArgs {
    /// The address of the contract to verify.
    pub address: Address,

    /// The contract identifier in the form `<path>:<contractname>`.
    pub contract: ContractInfo,

    /// The block at which the bytecode should be verified.
    #[clap(long, value_name = "BLOCK")]
    pub block: Option<BlockId>,

    /// The constructor args to generate the creation code.
    #[clap(
        long,
        num_args(1..),
        conflicts_with_all = &["constructor_args_path", "encoded_constructor_args"],
        value_name = "ARGS",
    )]
    pub constructor_args: Option<Vec<String>>,

    /// The ABI-encoded constructor arguments.
    #[arg(
        long,
        conflicts_with_all = &["constructor_args_path", "constructor_args"],
        value_name = "HEX",
    )]
    pub encoded_constructor_args: Option<String>,

    /// The path to a file containing the constructor arguments.
    #[arg(
        long,
        value_hint = ValueHint::FilePath,
        value_name = "PATH",
        conflicts_with_all = &["constructor_args", "encoded_constructor_args"]
    )]
    pub constructor_args_path: Option<PathBuf>,

    /// The rpc url to use for verification.
    #[clap(short = 'r', long, value_name = "RPC_URL", env = "ETH_RPC_URL")]
    pub rpc_url: Option<String>,

    #[clap(flatten)]
    pub etherscan: EtherscanOpts,

    /// Verifier options.
    #[clap(flatten)]
    pub verifier: VerifierArgs,

    /// Suppress logs and emit json results to stdout
    #[clap(long, default_value = "false")]
    pub json: bool,

    /// The project's root path.
    ///
    /// By default root of the Git repository, if in one,
    /// or the current working directory.
    #[arg(long, value_hint = ValueHint::DirPath, value_name = "PATH")]
    pub root: Option<PathBuf>,

    /// Ignore verification for creation or runtime bytecode.
    #[clap(long, value_name = "BYTECODE_TYPE")]
    pub ignore: Option<BytecodeType>,
}

impl figment::Provider for VerifyBytecodeArgs {
    fn metadata(&self) -> figment::Metadata {
        figment::Metadata::named("Verify Bytecode Provider")
    }

    fn data(
        &self,
    ) -> Result<figment::value::Map<figment::Profile, figment::value::Dict>, figment::Error> {
        let mut dict = self.etherscan.dict();
        if let Some(block) = &self.block {
            dict.insert("block".into(), figment::value::Value::serialize(block)?);
        }
        if let Some(rpc_url) = &self.rpc_url {
            dict.insert("eth_rpc_url".into(), rpc_url.to_string().into());
        }

        Ok(figment::value::Map::from([(Config::selected_profile(), dict)]))
    }
}

impl VerifyBytecodeArgs {
    /// Run the `verify-bytecode` command to verify the bytecode onchain against the locally built
    /// bytecode.
    pub async fn run(mut self) -> Result<()> {
        // Setup
        let config = self.load_config_emit_warnings();
        let provider = utils::get_provider(&config)?;

        // If chain is not set, we try to get it from the RPC.
        // If RPC is not set, the default chain is used.
        let chain = match config.get_rpc_url() {
            Some(_) => utils::get_chain(config.chain, &provider).await?,
            None => config.chain.unwrap_or_default(),
        };

        // Set Etherscan options.
        self.etherscan.chain = Some(chain);
        self.etherscan.key = config.get_etherscan_config_with_chain(Some(chain))?.map(|c| c.key);

        // Etherscan client
        let etherscan = EtherscanVerificationProvider.client(
            self.etherscan.chain.unwrap_or_default(),
            self.verifier.verifier_url.as_deref(),
            self.etherscan.key().as_deref(),
            &config,
        )?;

        // Get the bytecode at the address, bailing if it doesn't exist.
        let code = provider.get_code_at(self.address).await?;
        if code.is_empty() {
            eyre::bail!("No bytecode found at address {}", self.address);
        }

        if !self.json {
            println!(
                "Verifying bytecode for contract {} at address {}",
                self.contract.name.clone().green(),
                self.address.green()
            );
        }

        let mut json_results: Vec<JsonResult> = vec![];

        // Get creation tx hash.
        let creation_data = etherscan.contract_creation_data(self.address).await;

        // Check if contract is a predeploy
        let (creation_data, maybe_predeploy) =
            crate::utils::maybe_predeploy_contract(creation_data)?;

        trace!(maybe_predeploy = ?maybe_predeploy);

        // Get the constructor args using `source_code` endpoint.
        let source_code = etherscan.contract_source_code(self.address).await?;

        // Check if the contract name matches.
        let name = source_code.items.first().map(|item| item.contract_name.to_owned());
        if name.as_ref() != Some(&self.contract.name) {
            eyre::bail!("Contract name mismatch");
        }

        // Obtain Etherscan compilation metadata.
        let etherscan_metadata = source_code.items.first().unwrap();

        // Obtain local artifact
        let artifact = if let Ok(local_bytecode) =
            crate::utils::build_using_cache(&self, etherscan_metadata, &config)
        {
            trace!("using cache");
            local_bytecode
        } else {
            crate::utils::build_project(&self, &config)?
        };

        // Get local bytecode (creation code)
        let local_bytecode = artifact
            .bytecode
            .and_then(|b| b.into_bytes())
            .ok_or_eyre("Unlinked bytecode is not supported for verification")?;

        // Get the constructor args from etherscan
        let mut constructor_args = if let Some(args) = source_code.items.first() {
            args.constructor_arguments.clone()
        } else {
            eyre::bail!("No constructor arguments found for contract at address {}", self.address);
        };

        // Get and encode user provided constructor args
        let provided_constructor_args = if let Some(path) = self.constructor_args_path.to_owned() {
            // Read from file
            Some(read_constructor_args_file(path)?)
        } else {
            self.constructor_args.to_owned()
        }
        .map(|args| {
            if let Some(constructor) = artifact.abi.as_ref().and_then(|abi| abi.constructor()) {
                if constructor.inputs.len() != args.len() {
                    eyre::bail!(
                        "Mismatch of constructor arguments length. Expected {}, got {}",
                        constructor.inputs.len(),
                        args.len()
                    );
                }
                encode_args(&constructor.inputs, &args)
                    .map(|args| DynSolValue::Tuple(args).abi_encode())
            } else {
                Ok(Vec::new())
            }
        })
        .transpose()?
        .or(self.encoded_constructor_args.to_owned().map(hex::decode).transpose()?);

        if let Some(ref provided) = provided_constructor_args {
            constructor_args = provided.to_owned().into();
        }

        if maybe_predeploy {
            if !self.json {
                println!(
                    "{}",
                    format!("Attempting to verify predeployed contract at {:?}. Ignoring creation code verification.", self.address)
                        .yellow()
                        .bold()
                )
            }

            // If predeloy:
            // 1. Compile locally
            // 2. Get creation code from artifact.
            // 3. Append constructor args

            // Append constructor args to the local_bytecode.
            trace!(%constructor_args);
            let mut local_bytecode_vec = local_bytecode.to_vec();
            local_bytecode_vec.extend_from_slice(&constructor_args);

            // 4. Deploy at genesis
            let genesis_block_number = 0_u64;
            let (mut fork_config, evm_opts) = config.clone().load_config_and_evm_opts()?;
            fork_config.fork_block_number = Some(genesis_block_number);
            fork_config.evm_version =
                etherscan_metadata.evm_version()?.unwrap_or(EvmVersion::default());
            let (mut env, fork, _chain) =
                TracingExecutor::get_fork_material(&fork_config, evm_opts).await?;

            let mut executor = TracingExecutor::new(
                env.clone(),
                fork,
                Some(fork_config.evm_version),
                false,
                false,
            );

            env.block.number = U256::ZERO; // Genesis block
            let genesis_block =
                provider.get_block(genesis_block_number.into(), true.into()).await?;

            // Setup genesis tx and env.
            let deployer = Address::with_last_byte(0x1);
            let mut genesis_transaction = Transaction {
                from: deployer,
                to: None,
                input: Bytes::from(local_bytecode_vec),
                ..Default::default()
            };

            if let Some(ref block) = genesis_block {
                env.block.timestamp = U256::from(block.header.timestamp);
                env.block.coinbase = block.header.miner;
                env.block.difficulty = block.header.difficulty;
                env.block.prevrandao = Some(block.header.mix_hash.unwrap_or_default());
                env.block.basefee = U256::from(block.header.base_fee_per_gas.unwrap_or_default());
                env.block.gas_limit = U256::from(block.header.gas_limit);

                genesis_transaction.max_fee_per_gas =
                    Some(block.header.base_fee_per_gas.unwrap_or_default());
                genesis_transaction.gas = block.header.gas_limit;
                genesis_transaction.gas_price =
                    Some(block.header.base_fee_per_gas.unwrap_or_default());
            }

            configure_tx_env(&mut env, &genesis_transaction);

            // Seed deployer account with funds
            let account_info = AccountInfo {
                balance: U256::from(100 * 10_u128.pow(18)),
                nonce: 0,
                ..Default::default()
            };
            executor.backend_mut().insert_account_info(deployer, account_info);

            let env_with_handler = EnvWithHandlerCfg::new(
                Box::new(env.clone()),
                HandlerCfg::new(config.evm_spec_id()),
            );

            // Deploy contract
            let deploy_result = executor.deploy_with_env(env_with_handler, None)?;
            trace!(deploy_result = ?deploy_result.raw.exit_reason);
            let deployed_address = deploy_result.address;

            // Compare runtime bytecode
            let deployed_bytecode = executor
                .backend_mut()
                .basic(deployed_address)?
                .ok_or_else(|| {
                    eyre::eyre!(
                        "Failed to get runtime code for contract deployed on fork at address {}",
                        deployed_address
                    )
                })?
                .code
                .ok_or_else(|| {
                    eyre::eyre!(
                        "Bytecode does not exist for contract deployed on fork at address {}",
                        deployed_address
                    )
                })?;

            let onchain_runtime_code = provider.get_code_at(self.address).await?;

            let match_type = crate::utils::match_bytecodes(
                &deployed_bytecode.original_bytes(),
                &onchain_runtime_code,
                &Bytes::default(),
                true,
            );

            crate::utils::print_result(
                &self,
                match_type,
                BytecodeType::Runtime,
                &mut json_results,
                etherscan_metadata,
                &config,
            );

            if self.json {
                println!("{}", serde_json::to_string(&json_results)?);
            }

            return Ok(());
        }

        let creation_data = creation_data.unwrap(); // We can unwrap directly as maybe_predeploy is false

        // Get transaction and receipt.
        trace!(creation_tx_hash = ?creation_data.transaction_hash);
        let mut transaction = provider
            .get_transaction_by_hash(creation_data.transaction_hash)
            .await
            .or_else(|e| eyre::bail!("Couldn't fetch transaction from RPC: {:?}", e))?
            .ok_or_else(|| {
                eyre::eyre!("Transaction not found for hash {}", creation_data.transaction_hash)
            })?;
        let receipt = provider
            .get_transaction_receipt(creation_data.transaction_hash)
            .await
            .or_else(|e| eyre::bail!("Couldn't fetch transaction receipt from RPC: {:?}", e))?;
        let receipt = if let Some(receipt) = receipt {
            receipt
        } else {
            eyre::bail!(
                "Receipt not found for transaction hash {}",
                creation_data.transaction_hash
            );
        };

        // Extract creation code from creation tx input.
        let maybe_creation_code =
            if receipt.to.is_none() && receipt.contract_address == Some(self.address) {
                &transaction.input
            } else if receipt.to == Some(DEFAULT_CREATE2_DEPLOYER) {
                &transaction.input[32..]
            } else {
                eyre::bail!(
                    "Could not extract the creation code for contract at address {}",
                    self.address
                );
            };

        if let Some(provided) = provided_constructor_args {
            constructor_args = provided.into();
        } else {
            // In some cases, Etherscan will return incorrect constructor arguments. If this
            // happens, try extracting arguments ourselves.
            if !maybe_creation_code.ends_with(&constructor_args) {
                trace!("mismatch of constructor args with etherscan");
                // If local bytecode is longer than on-chain one, this is probably not a match.
                if maybe_creation_code.len() >= local_bytecode.len() {
                    constructor_args =
                        Bytes::copy_from_slice(&maybe_creation_code[local_bytecode.len()..]);
                    trace!(
                        target: "forge::verify",
                        "setting constructor args to latest {} bytes of bytecode",
                        constructor_args.len()
                    );
                }
            }
        }

        // Append constructor args to the local_bytecode.
        trace!(%constructor_args);
        let mut local_bytecode_vec = local_bytecode.to_vec();
        local_bytecode_vec.extend_from_slice(&constructor_args);

        trace!(ignore = ?self.ignore);
        // Check if `--ignore` is set to `creation`.
        if !self.ignore.is_some_and(|b| b.is_creation()) {
            // Compare creation code with locally built bytecode and `maybe_creation_code`.
            let match_type = crate::utils::match_bytecodes(
                local_bytecode_vec.as_slice(),
                maybe_creation_code,
                &constructor_args,
                false,
            );

            crate::utils::print_result(
                &self,
                match_type,
                BytecodeType::Creation,
                &mut json_results,
                etherscan_metadata,
                &config,
            );

            // If the creation code does not match, the runtime also won't match. Hence return.
            if match_type.is_none() {
                crate::utils::print_result(
                    &self,
                    None,
                    BytecodeType::Runtime,
                    &mut json_results,
                    etherscan_metadata,
                    &config,
                );
                if self.json {
                    println!("{}", serde_json::to_string(&json_results)?);
                }
                return Ok(());
            }
        }

        if !self.ignore.is_some_and(|b| b.is_runtime()) {
            // Get contract creation block.
            let simulation_block = match self.block {
                Some(BlockId::Number(BlockNumberOrTag::Number(block))) => block,
                Some(_) => eyre::bail!("Invalid block number"),
                None => {
                    let provider = utils::get_provider(&config)?;
                    provider
                    .get_transaction_by_hash(creation_data.transaction_hash)
                    .await.or_else(|e| eyre::bail!("Couldn't fetch transaction from RPC: {:?}", e))?.ok_or_else(|| {
                        eyre::eyre!("Transaction not found for hash {}", creation_data.transaction_hash)
                    })?
                    .block_number.ok_or_else(|| {
                        eyre::eyre!("Failed to get block number of the contract creation tx, specify using the --block flag")
                    })?
                }
            };

            // Fork the chain at `simulation_block`.
            let (mut fork_config, evm_opts) = config.clone().load_config_and_evm_opts()?;
            fork_config.fork_block_number = Some(simulation_block - 1);
            fork_config.evm_version =
                etherscan_metadata.evm_version()?.unwrap_or(EvmVersion::default());
            let (mut env, fork, _chain) =
                TracingExecutor::get_fork_material(&fork_config, evm_opts).await?;

            let mut executor = TracingExecutor::new(
                env.clone(),
                fork,
                Some(fork_config.evm_version),
                false,
                false,
            );
            env.block.number = U256::from(simulation_block);
            let block = provider.get_block(simulation_block.into(), true.into()).await?;

            // Workaround for the NonceTooHigh issue as we're not simulating prior txs of the same
            // block.
            let prev_block_id = BlockId::number(simulation_block - 1);

            // Use `transaction.from` instead of `creation_data.contract_creator` to resolve
            // blockscout creation data discrepancy in case of CREATE2.
            let prev_block_nonce =
                provider.get_transaction_count(transaction.from).block_id(prev_block_id).await?;
            transaction.nonce = prev_block_nonce;

            if let Some(ref block) = block {
                env.block.timestamp = U256::from(block.header.timestamp);
                env.block.coinbase = block.header.miner;
                env.block.difficulty = block.header.difficulty;
                env.block.prevrandao = Some(block.header.mix_hash.unwrap_or_default());
                env.block.basefee = U256::from(block.header.base_fee_per_gas.unwrap_or_default());
                env.block.gas_limit = U256::from(block.header.gas_limit);
            }

            // Replace the `input` with local creation code in the creation tx.
            if let Some(to) = transaction.to {
                if to == DEFAULT_CREATE2_DEPLOYER {
                    let mut input = transaction.input[..32].to_vec(); // Salt
                    input.extend_from_slice(&local_bytecode_vec);
                    transaction.input = Bytes::from(input);

                    // Deploy default CREATE2 deployer
                    executor.deploy_create2_deployer()?;
                }
            } else {
                transaction.input = Bytes::from(local_bytecode_vec);
            }

            configure_tx_env(&mut env, &transaction);

            let env_with_handler = EnvWithHandlerCfg::new(
                Box::new(env.clone()),
                HandlerCfg::new(config.evm_spec_id()),
            );

            let contract_address = if let Some(to) = transaction.to {
                if to != DEFAULT_CREATE2_DEPLOYER {
                    eyre::bail!("Transaction `to` address is not the default create2 deployer i.e the tx is not a contract creation tx.");
                }
                let result = executor.transact_with_env(env_with_handler.clone())?;

                if result.result.len() != 20 {
                    eyre::bail!("Failed to deploy contract on fork at block {simulation_block}: call result is not exactly 20 bytes");
                }

                Address::from_slice(&result.result)
            } else {
                let deploy_result = executor.deploy_with_env(env_with_handler, None)?;
                deploy_result.address
            };

            // State commited using deploy_with_env, now get the runtime bytecode from the db.
            let fork_runtime_code = executor
                .backend_mut()
                .basic(contract_address)?
                .ok_or_else(|| {
                    eyre::eyre!(
                        "Failed to get runtime code for contract deployed on fork at address {}",
                        contract_address
                    )
                })?
                .code
                .ok_or_else(|| {
                    eyre::eyre!(
                        "Bytecode does not exist for contract deployed on fork at address {}",
                        contract_address
                    )
                })?;

            let onchain_runtime_code = provider
                .get_code_at(self.address)
                .block_id(BlockId::number(simulation_block))
                .await?;

            // Compare the onchain runtime bytecode with the runtime code from the fork.
            let match_type = crate::utils::match_bytecodes(
                &fork_runtime_code.original_bytes(),
                &onchain_runtime_code,
                &constructor_args,
                true,
            );

            crate::utils::print_result(
                &self,
                match_type,
                BytecodeType::Runtime,
                &mut json_results,
                etherscan_metadata,
                &config,
            );
        }

        if self.json {
            println!("{}", serde_json::to_string(&json_results)?);
        }
        Ok(())
    }
}
