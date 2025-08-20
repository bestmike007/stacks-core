mod simulate;
mod storage;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_arch = "arm")))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_arch = "arm")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

use crate::simulate::nakamoto_simulate::nakamoto_simulate;

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() == 4 {
        if argv[1] == "resimulate-nakamoto-block" {
            storage::ensure_nakamoto_block_indexes().unwrap();
            let r = nakamoto_simulate(
                u64::from_str_radix(&argv[2], 10)
                    .expect(&format!("{} is not a valid number", &argv[2])),
                &argv[3],
            );
            match r {
                Ok(result) => {
                    println!("Resimulate nakamoto block success: {:?}", result);
                }
                Err(e) => {
                    println!("Resimulate nakamoto block failed with err: {}", e);
                }
            }
            return;
        }
    }
    println!(
        "Stacks block re-simulation usage: resimulate resimulate-nakamoto-block <block-height> <block-hash>"
    );
    println!(
        "Example: STACKS_NODE_DIR=$PWD/stacks-node/mainnet ./resimulate resimulate-nakamoto-block 2909744 e58905cfa3258df3e6c0dcd1b321fcf67d46fd83de1b1a7e85eea5d567cfdf64"
    );
    return;
}
