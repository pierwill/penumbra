use std::{fs::File, io::Write, path::PathBuf};

use anyhow::{anyhow, Context as _, Result};
use comfy_table::{presets, Table};
use directories::ProjectDirs;
use penumbra_crypto::{
    asset::{self, Denom},
    keys::SpendSeed,
    Value, CURRENT_CHAIN_ID,
};
use penumbra_wallet::{ClientState, UnspentNote, Wallet};
use rand_core::OsRng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use structopt::StructOpt;
use tempfile::NamedTempFile;

pub mod opt;
pub mod warning;
use opt::*;

mod sync;
pub use sync::sync;

pub mod fetch;

mod state;
pub use state::ClientStateFile;

#[tokio::main]
async fn main() -> Result<()> {
    // Display a warning message to the user so they don't get upset when all their tokens are lost.
    if std::env::var("PCLI_UNLEASH_DANGER").is_err() {
        warning::display();
    }

    tracing_subscriber::fmt::init();
    let opt = Opt::from_args();

    let project_dir =
        ProjectDirs::from("zone", "penumbra", "pcli").expect("can access penumbra project dir");
    // Currently we use just the data directory. Create it if it is missing.
    std::fs::create_dir_all(project_dir.data_dir()).expect("can create penumbra data directory");

    // We store wallet data in `penumbra_wallet.dat` in the state directory, unless
    // the user provides another location.
    let wallet_path = opt.wallet_location.map_or_else(
        || project_dir.data_dir().join("penumbra_wallet.json"),
        PathBuf::from,
    );

    // Synchronize the wallet if the command requires it to be synchronized before it is run.
    let state = if opt.cmd.needs_sync() {
        let mut state = ClientStateFile::load(wallet_path.clone())?;
        let light_wallet_server_uri = format!("http://{}:{}", opt.node, opt.light_wallet_port);
        let thin_wallet_server_uri = format!("http://{}:{}", opt.node, opt.thin_wallet_port);
        sync(&mut state, light_wallet_server_uri).await?;
        fetch::assets(&mut state, thin_wallet_server_uri).await?;
        Some(state)
    } else {
        None
    };

    match opt.cmd {
        Command::Sync => {
            // We have already synchronized the wallet above, so we can just return.
        }
        Command::Tx(TxCmd::Send {
            values,
            to,
            fee,
            from,
            memo,
        }) => {
            // Parse all of the values provided.
            let values = values
                .iter()
                .map(|v| v.parse())
                .collect::<Result<Vec<Value>, _>>()?;
            let to = to
                .parse()
                .map_err(|_| anyhow::anyhow!("address is invalid"))?;

            let mut state = state.expect("state must be synchronized");
            let tx = state.new_transaction(&mut OsRng, &values, fee, to, from, memo)?;
            state.commit()?;

            let serialized_tx: Vec<u8> = tx.into();

            tracing::info!("broadcasting transaction...");
            let rsp = reqwest::get(format!(
                r#"http://{}:{}/broadcast_tx_sync?tx=0x{}"#,
                opt.node,
                opt.rpc_port,
                hex::encode(serialized_tx)
            ))
            .await?
            .text()
            .await?;

            tracing::info!("{}", rsp);
        }
        Command::Wallet(wallet_cmd) => {
            // Dispatch on the wallet command and return a new state if the command required a
            // wallet state to be saved to disk
            let state = match wallet_cmd {
                // These two commands return new wallets to be saved to disk:
                WalletCmd::Generate => Some(ClientState::new(Wallet::generate(&mut OsRng))),
                WalletCmd::Import { spend_seed } => {
                    let seed = hex::decode(spend_seed)?;
                    let seed = SpendSeed::try_from(seed.as_slice())?;
                    Some(ClientState::new(Wallet::import(seed)))
                }
                // The rest of these commands don't require a wallet state to be saved to disk:
                WalletCmd::Export => {
                    let state = ClientStateFile::load(wallet_path.clone())?;
                    let seed = state.wallet().spend_key().seed().clone();
                    println!("{}", hex::encode(&seed.0));
                    None
                }
                WalletCmd::Delete => {
                    if wallet_path.is_file() {
                        std::fs::remove_file(&wallet_path)?;
                        println!("Deleted wallet file at {}", wallet_path.display());
                    } else if wallet_path.exists() {
                        return Err(anyhow!(
                            "Expected wallet file at {} but found something that is not a file; refusing to delete it",
                            wallet_path.display()
                        ));
                    } else {
                        return Err(anyhow!(
                            "No wallet exists at {}, so it cannot be deleted",
                            wallet_path.display()
                        ));
                    }
                    None
                }
                WalletCmd::Reset => {
                    tracing::info!("resetting client state");

                    #[derive(Deserialize)]
                    struct MinimalState {
                        wallet: Wallet,
                    }

                    // Read the wallet field out of the state file, without fully deserializing the rest
                    let wallet =
                        serde_json::from_reader::<_, MinimalState>(File::open(&wallet_path)?)?
                            .wallet;

                    // Write the new wallet JSON to disk as a temporary file
                    let (mut tmp, tmp_path) = NamedTempFile::new()?.into_parts();
                    tmp.write_all(
                        serde_json::to_string_pretty(&ClientState::new(wallet))?.as_bytes(),
                    )?;

                    // Check that we can successfully parse the result from disk
                    ClientStateFile::load(tmp_path.to_path_buf()).context("can't parse wallet after attempting to reset: refusing to overwrite existing wallet file")?;

                    // Move the temporary file over the original wallet file
                    tmp_path.persist(&wallet_path)?;

                    None
                }
            };

            // If a new wallet should be saved to disk, save it and also archive it in the archive directory
            if let Some(state) = state {
                // Never overwrite a wallet that already exists
                if wallet_path.exists() {
                    return Err(anyhow::anyhow!(
                        "Wallet path {} already exists, refusing to overwrite it",
                        wallet_path.display()
                    ));
                }

                println!("Saving wallet to {}", wallet_path.display());
                ClientStateFile::save(state.clone(), wallet_path)?;

                // Archive the newly generated state
                let archive_dir = ProjectDirs::from("zone", "penumbra", "penumbra-testnet-archive")
                    .expect("can access penumbra-testnet-archive dir");

                // Create the directory <data dir>/penumbra-testnet-archive/<chain id>/<spend key hash prefix>/
                let spend_key_hash = Sha256::digest(&state.wallet().spend_key().seed().0);
                let wallet_archive_dir = archive_dir
                    .data_dir()
                    .join(CURRENT_CHAIN_ID)
                    .join(hex::encode(&spend_key_hash[0..8]));
                std::fs::create_dir_all(&wallet_archive_dir)
                    .expect("can create penumbra wallet archive directory");

                // Save the wallet file in the archive directory
                let archive_path = wallet_archive_dir.join("penumbra_wallet.json");
                println!("Saving backup wallet to {}", archive_path.display());
                ClientStateFile::save(state, archive_path)?;
            }
        }
        Command::Addr(addr_cmd) => {
            let mut state = ClientStateFile::load(wallet_path)?;

            // Set up table (this won't be used with `show --addr-only`)
            let mut table = Table::new();
            table.load_preset(presets::NOTHING);
            table.set_header(vec!["Index", "Label", "Address"]);

            match addr_cmd {
                AddrCmd::List => {
                    for (index, label, address) in state.wallet().addresses() {
                        table.add_row(vec![index.to_string(), label, address.to_string()]);
                    }
                }
                AddrCmd::Show { index, addr_only } => {
                    let (label, address) = state.wallet().address_by_index(index as usize)?;

                    if addr_only {
                        println!("{}", address.to_string());
                        return Ok(()); // don't print the label
                    } else {
                        table.add_row(vec![index.to_string(), label, address.to_string()]);
                    }
                }
                AddrCmd::New { label } => {
                    let (index, address, _dtk) = state.wallet_mut().new_address(label.clone());
                    state.commit()?;
                    table.add_row(vec![index.to_string(), label, address.to_string()]);
                }
            }

            // Print the table (we don't get here if `show --addr-only`)
            println!("{}", table);
        }
        Command::Balance {
            by_address,
            offline,
        } => {
            // Format a tally of notes as three strings: total, unspent, and pending spend. This
            // assumes that the notes are all of the same denomination, and it is called below only
            // in the places where they are.
            fn tally_format_notes<'a>(
                denom: &Denom,
                cache: &asset::Cache,
                notes: impl IntoIterator<Item = UnspentNote<'a>>,
            ) -> (String, String, String, String) {
                // Tally each of the kinds of note:
                let mut unspent = 0;
                let mut pending = 0;
                let mut pending_change = 0;

                for note in notes {
                    *match note {
                        UnspentNote::Ready(_) => &mut unspent,
                        UnspentNote::PendingSpend(_) => &mut pending,
                        UnspentNote::PendingChange(_) => &mut pending_change,
                    } += note.as_ref().amount();
                }

                // The amount spent is the difference between pending and pending change:
                let pending_spend = pending - pending_change;

                let pending_change = denom.value(pending_change);
                let pending_spend = denom.value(pending_spend);

                let pending_change_string = if pending_change.amount > 0 {
                    format!("+{} (change)", pending_change.try_format(cache).unwrap())
                } else {
                    "".to_string()
                };

                let pending_spend_string = if pending_spend.amount > 0 {
                    format!("-{} (spend)", pending_spend.try_format(cache).unwrap())
                } else {
                    "".to_string()
                };

                // The total amount, disregarding pending transactions:
                let total = denom.value(pending_change.amount + unspent);
                // The amount available to spend:
                let available = denom.value(unspent);

                (
                    total.try_format(cache).unwrap(),
                    available.try_format(cache).unwrap(),
                    pending_change_string,
                    pending_spend_string,
                )
            }

            // Load the synchronized wallet state, or else load from disk if in offline mode
            let state = if !offline {
                state.expect("state must be synchronized")
            } else {
                ClientStateFile::load(wallet_path)?
            };

            // Initialize the table
            let mut table = Table::new();
            table.load_preset(presets::NOTHING);
            let mut print_pending_column = false; // This will become true if there are any pending transactions
            let mut headers;

            if by_address {
                for (address_id, by_denom) in state.unspent_notes_by_address_and_denom().into_iter()
                {
                    let (mut label, _) = state.wallet().address_by_index(address_id as usize)?;
                    for (denom, notes) in by_denom.into_iter() {
                        let (total, available, pending_change, pending_spend) =
                            tally_format_notes(&denom, state.asset_cache(), notes);
                        let mut row = vec![label.clone(), total];
                        if !pending_change.is_empty() || !pending_spend.is_empty() {
                            print_pending_column = true;
                            row.push(available);
                            row.push(pending_change);
                            row.push(pending_spend);
                        }
                        table.add_row(row);

                        // Only display the label on the first row
                        label = String::default();
                    }
                }

                // Set up headers for the table (a "Pending" column will be added if there are any
                // pending transactions)
                headers = vec!["Address", "Total"];
            } else {
                for (denom, by_address) in state.unspent_notes_by_denom_and_address().into_iter() {
                    let (total, available, pending_change, pending_spend) = tally_format_notes(
                        &denom,
                        state.asset_cache(),
                        by_address.into_values().flatten(),
                    );
                    let mut row = vec![total];
                    if !pending_change.is_empty() || !pending_spend.is_empty() {
                        print_pending_column = true;
                        row.push(available);
                        row.push(pending_change);
                        row.push(pending_spend);
                    }
                    table.add_row(row);
                }

                // Set up headers for the table (a "Pending" column will be added if there are any
                // pending transactions)
                headers = vec!["Total"];
            }

            // Add an "Available" and "Pending" column if there are any pending transactions
            if print_pending_column {
                headers.push("Available");
                headers.push("Pending");
            }
            table.set_header(headers);
            println!("{}", table);
        }
    }

    Ok(())
}
