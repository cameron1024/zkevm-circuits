mod abi;
mod code_cache;
mod lllc;
mod result_cache;
mod statetest;
mod utils;
mod yaml;

use crate::lllc::Lllc;
use crate::yaml::YamlStateTestBuilder;
use anyhow::{bail, Result};
use clap::Parser;
use eth_types::{evm_types::Gas, U256};
use rayon::prelude::*;
use result_cache::ResultCache;
use statetest::{StateTest, StateTestConfig, StateTestError};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use zkevm_circuits::test_util::BytecodeTestConfig;

use crate::utils::config_bytecode_test_config;

#[macro_use]
extern crate prettytable;
use prettytable::Table;

/// EVM test vectors utility
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Path of test files
    #[clap(
        short,
        long,
        default_value = "tests/src/GeneralStateTestsFiller/VMTests/**"
    )]
    path: String,

    /// Test to execute
    #[clap(short, long)]
    test: Option<String>,

    /// Do not run circuits
    #[clap(long, short)]
    skip_circuit: bool,

    /// Do not run circuit
    #[clap(long)]
    skip_state_circuit: bool,

    /// Raw execute bytecode
    #[clap(short, long)]
    raw: Option<String>,
}

const TEST_IGNORE_LIST : [&str;1] = ["gasCostMemory_d61_g0_v0"];
const FILE_IGNORE_LIST : [&str;4]=  [
        "EIP1559",
        "EIP2930",
        "stExample",
        "ValueOverflowFiller", // weird 0x:biginteger 0x...
    ];




/// This crate helps to execute the common ethereum tests located in https://github.com/ethereum/tests

const RESULT_CACHE: &str = "result.cache";

fn run_test_suite(tcs: Vec<StateTest>, config: StateTestConfig) -> Result<()> {
    let results = ResultCache::new(PathBuf::from(RESULT_CACHE))?;

    let tcs: Vec<StateTest> = tcs
        .into_iter()
        .filter(|t| !results.contains(&t.id))
        .collect();

    let results = Arc::new(RwLock::from(results));


    // for each test
    tcs.into_par_iter().for_each(|tc| {
        let id = tc.id.clone();
        if TEST_IGNORE_LIST.contains(&id.as_str()) {
            return;
        }
        if results.read().unwrap().contains(&id.as_str()) {
            return;
        }

        log::info!("Running {}",id);
        std::panic::set_hook(Box::new(|_info| {}));
        let result = std::panic::catch_unwind(|| tc.run(config.clone()));

        // handle panic
        let result = match result {
            Ok(res) => res,
            Err(_) => {
                log::error!(target: "vmvectests", "PANIKED test {}",id);
                return;
            }
        };

        // handle known error
        if let Err(err) = result {
            match err {
                StateTestError::SkipUnimplementedOpcode(_)
                | StateTestError::SkipTestMaxGasLimit(_) => {
                    log::warn!(target: "vmvectests", "SKIPPED test {} : {:?}",id, err);
                    results
                        .write()
                        .unwrap()
                        .insert(&id, &format!("{}", err))
                        .unwrap();
                }
                _ => log::error!(target: "vmvectests", "FAILED test {} : {:?}",id, err),
            }
            return;
        }

        let results = std::sync::Arc::clone(&results);
        results.write().unwrap().insert(&id, "success").unwrap();
        log::info!(target: "vmvectests", "SUCCESS test {}",id)
    });

    Ok(())
}

fn run_single_test(test: StateTest, mut config: StateTestConfig) -> Result<()> {
    println!("{}", &test);

    fn kv(storage: std::collections::HashMap<U256, U256>) -> Vec<String> {
        let mut keys: Vec<_> = storage.keys().collect();
        keys.sort();
        keys.iter()
            .map(|k| format!("{:?}: {:?}", k, storage[k]))
            .collect()
    }
    fn split(strs: Vec<String>, len: usize) -> String {
        let mut out = String::new();
        let mut current = 0;
        for s in strs {
            if current > len {
                current = 0;
                out.push('\n');
            } else if current > 0 {
                out.push_str(", ");
            }
            out.push_str(&s);
            current += s.len();
        }
        out
    }

    let trace = test.clone().geth_trace()?;

    config_bytecode_test_config(
        &mut config.bytecode_test_config,
        trace.struct_logs.iter().map(|step| step.op),
    );

    let mut table = Table::new();
    table.add_row(row![
        "PC", "OP", "GAS", "GAS_COST", "DEPTH", "ERR", "STACK", "MEMORY", "STORAGE"
    ]);
    for step in trace.struct_logs {
        table.add_row(row![
            format!("{}", step.pc.0),
            format!("{:?}", step.op),
            format!("{}", step.gas.0),
            format!("{}", step.gas_cost.0),
            format!("{}", step.depth),
            step.error.unwrap_or("".to_string()),
            split(step.stack.0.iter().map(ToString::to_string).collect(), 30),
            split(step.memory.0.iter().map(ToString::to_string).collect(), 30),
            split(kv(step.storage.0), 30)
        ]);
    }

    println!("FAILED: {:?}", trace.failed);
    println!("GAS: {:?}", trace.gas);
    table.printstd();
    println!("result={:?}", test.run(config));

    Ok(())
}

fn run_bytecode(code: &str, mut bytecode_test_config: BytecodeTestConfig) -> Result<()> {
    use eth_types::bytecode;
    use mock::TestContext;
    use std::str::FromStr;
    use zkevm_circuits::test_util::run_test_circuits;

    let bytecode = if let Ok(bytes) = hex::decode(code) {
        let bytecode = bytecode::Bytecode::try_from(bytes).expect("unable to decode bytecode");
        for op in bytecode.iter() {
            println!("{}", op.to_string());
        }
        bytecode
    } else {
        let mut bytecode = bytecode::Bytecode::default();
        for op in code.split(";") {
            let op = bytecode::OpcodeWithData::from_str(op.trim()).unwrap();
            bytecode.append_op(op);
        }
        println!("{}\n", hex::encode(bytecode.code()));
        bytecode
    };

    config_bytecode_test_config(
        &mut bytecode_test_config,
        bytecode.iter().map(|op| op.opcode()),
    );

    let result = run_test_circuits(
        TestContext::<2, 1>::simple_ctx_with_bytecode(bytecode)?,
        Some(bytecode_test_config),
    );

    println!("Execution result is : {:?}", result);

    Ok(())
}

fn main() -> Result<()> {

    //  RAYON_NUM_THREADS=1 RUST_BACKTRACE=1 cargo run -- --path "tests/src/GeneralStateTestsFiller/**" --skip-state-circuit
    
    let args = Args::parse();

    let bytecode_test_config = BytecodeTestConfig {
        enable_state_circuit_test: !args.skip_state_circuit,
        ..Default::default()
    };

    if let Some(raw) = &args.raw {
        run_bytecode(&raw, bytecode_test_config)?;
        return Ok(());
    }

    ResultCache::new(PathBuf::from(RESULT_CACHE))?.sort()?;

    let config = StateTestConfig {
        max_gas: Gas(1000000),
        run_circuit: !args.skip_circuit,
        bytecode_test_config,
    };

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let files = glob::glob(&format!("{}/*.yml", args.path))
        .expect("Failed to read glob pattern")
        .map(|f| f.unwrap())
        .filter(|f| !FILE_IGNORE_LIST.iter().any(|e| f.as_path().to_string_lossy().contains(e)));

    let mut tests = Vec::new();
    let mut lllc = Lllc::default().with_docker_lllc().with_default_cache()?;

    log::info!("Parsing and compliling tests...");
    for file in files {
        let src = std::fs::read_to_string(&file)?;
        let path = file.as_path().to_string_lossy();
        println!("======>{}",path);
        let mut tcs = match YamlStateTestBuilder::new(&mut lllc).from_yaml(&path, &src) {
            Err(err) => {
                log::warn!("Failed to load {}: {:?}", path, err);
                Vec::new()
            }
            Ok(tcs) => tcs,
        };
        tests.append(&mut tcs);
    }

    if let Some(test_id) = args.test {
        tests = tests.into_iter().filter(|t| t.id == test_id).collect();
        if tests.is_empty() {
            bail!("test '{}' not found", test_id);
        }
        let test = tests.remove(0);
        run_single_test(test, config)?;
    } else {
        run_test_suite(tests, config)?;
    }

    Ok(())
}
