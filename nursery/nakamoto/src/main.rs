use keychain_tracker_example_cli::{
    anyhow,
    clap::{self, Args, Subcommand, ValueEnum},
    handle_commands, init, Commands,
};

use crate::cbf::CbfEvent;
use bdk_chain::chain_graph::ChainGraph;
use bdk_chain::keychain::{DerivationAdditions, KeychainChangeSet};
use bdk_chain::{BlockId, TxHeight};

use nakamoto::client::network::Services;
use nakamoto::client::traits::Handle;
use nakamoto::client::Handle as ClientHandle;

mod cbf;

/*
#[derive(Args, Debug, Clone)]
struct CbfArgs {
    #[arg(value_enum, default_value_t = Domains::IPv4)]
    domains: Domains,
}
*/

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Domains {
    IPv4,
    IPv6,
    Both,
}

#[derive(Subcommand, Debug, Clone)]
enum CbfCommands {
    Rescan {
        #[clap(long, default_value = "1000")]
        _batch_size: u32,
    },
    Scan,
}

fn main() -> anyhow::Result<()> {
    println!("Loading wallet from db...");
    let (args, keymap, mut keychain_tracker, mut db) = init::<CbfCommands, BlockId, TxHeight>()?;
    println!("Wallet loaded.");

    let mut client = cbf::CbfClient::new(
        args.network.into(),
        /*args.chain_args.domains*/ Domains::IPv4,
    )?;

    let cbf_cmd = match args.command {
        Commands::ChainSpecific(cbf_cmd) => cbf_cmd,
        general_cmd => {
            return handle_commands(
                general_cmd,
                |transaction| {
                    println!("Looking for peers...");
                    client.handle.wait_for_peers(1, Services::default())?;
                    println!("Connected to at least one peer");

                    client.handle.submit_transaction(transaction.clone())?;
                    Ok(())
                },
                &mut keychain_tracker,
                &mut db,
                args.network,
                &keymap,
            );
        }
    };

    match cbf_cmd {
        CbfCommands::Rescan { _batch_size } => {
            todo!("Implement rescan from sync");
            // This function will reveal batch_size scripts and rescan the nakamoto
            // client
            // If we notice, after rescan, that more than `batch_size - epsilon` scripts
            // have been used, we rescan using 2*batch_size, and so on
        }
        CbfCommands::Scan => {
            // indexing logic!
            let mut keychain_tracker = keychain_tracker.lock().unwrap();

            // find scripts!
            let scripts = keychain_tracker
                .txout_index
                .inner()
                .all_spks()
                .clone()
                .into_values();

            let last_checkpoint = keychain_tracker.chain_graph().chain().latest_checkpoint();

            let mut update = ChainGraph::default();

            let mut derivation_additions = DerivationAdditions::default();

            client.sync_setup(scripts, last_checkpoint)?;
            loop {
                match client.next_event()? {
                    CbfEvent::BlockMatched(id, txs) => {
                        for (tx, height) in txs {
                            if keychain_tracker.txout_index.is_relevant(&tx) {
                                println!("* adding tx to update: {} @ {}", tx.txid(), height);
                                let _ = update.insert_tx(tx.clone(), height)?;
                            }
                            derivation_additions.append(keychain_tracker.txout_index.scan(&tx));
                        }
                    }
                    CbfEvent::ChainSynced(h) => {
                        break;
                    }
                }
            }

            let changeset = KeychainChangeSet {
                derivation_indices: derivation_additions,
                chain_graph: keychain_tracker
                    .chain_graph()
                    .determine_changeset(&update)?,
            };
            db.lock().unwrap().append_changeset(&changeset)?;
            keychain_tracker.apply_changeset(changeset);
            Ok(())
        }
    }
}
