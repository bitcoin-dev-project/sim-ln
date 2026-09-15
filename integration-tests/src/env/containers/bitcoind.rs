//! The bitcoind container backing the regtest network: chain source for every node, miner, and
//! on-chain wallet (which Eclair also spends from directly, since it has no wallet of its own).

use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use bitcoin::Address;
use bitcoincore_rpc::{Auth, Client, RpcApi};
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use super::{dump_logs, BTC_RPC_PASS, BTC_RPC_USER};
use crate::retry::{with_backoff, Backoff};

/// Multi-arch maintainer-published Bitcoin Core image. Eclair 0.14.x requires Core 31+.
const IMAGE: &str = "bitcoin/bitcoin";
const TAG: &str = "31.1";

/// Regtest RPC port inside the container.
const RPC_PORT: u16 = 18443;

/// ZMQ endpoints: LND consumes rawblock + rawtx, Eclair consumes hashblock + rawtx.
pub const ZMQ_RAWBLOCK_PORT: u16 = 28332;
pub const ZMQ_RAWTX_PORT: u16 = 28333;
pub const ZMQ_HASHBLOCK_PORT: u16 = 28334;

const WALLET: &str = "simln";

pub struct Bitcoind {
    container: ContainerAsync<GenericImage>,
    name: String,
    wallet_rpc: Client,
}

impl Bitcoind {
    pub async fn start(docker_network: &str) -> anyhow::Result<Self> {
        let name = format!("simln-bitcoind-{}", std::process::id());
        let container = GenericImage::new(IMAGE, TAG)
            .with_exposed_port(RPC_PORT.tcp())
            .with_network(docker_network)
            .with_container_name(&name)
            .with_startup_timeout(Duration::from_secs(600))
            .with_cmd([
                "-regtest",
                "-server=1",
                "-txindex=1",
                "-rpcbind=0.0.0.0",
                "-rpcallowip=0.0.0.0/0",
                &format!("-rpcuser={BTC_RPC_USER}"),
                &format!("-rpcpassword={BTC_RPC_PASS}"),
                &format!("-zmqpubrawblock=tcp://0.0.0.0:{ZMQ_RAWBLOCK_PORT}"),
                &format!("-zmqpubrawtx=tcp://0.0.0.0:{ZMQ_RAWTX_PORT}"),
                &format!("-zmqpubhashblock=tcp://0.0.0.0:{ZMQ_HASHBLOCK_PORT}"),
                "-fallbackfee=0.0002",
                "-addresstype=bech32m",
                "-changetype=bech32m",
            ])
            .start()
            .await
            .context("starting bitcoind container")?;

        let host_port = container.get_host_port_ipv4(RPC_PORT).await?;
        let auth = Auth::UserPass(BTC_RPC_USER.to_string(), BTC_RPC_PASS.to_string());
        let base_rpc = Client::new(&format!("http://127.0.0.1:{host_port}"), auth.clone())?;

        let setup = async {
            with_backoff("bitcoind rpc ready", Backoff::default(), || async {
                // Untyped call: the crate's typed getblockchaininfo struct predates Core 31's
                // response format (warnings became an array).
                base_rpc.call::<serde_json::Value>("getblockchaininfo", &[])
            })
            .await?;

            base_rpc
                .create_wallet(WALLET, None, None, None, None)
                .context("creating bitcoind wallet")?;

            let wallet_rpc = Client::new(
                &format!("http://127.0.0.1:{host_port}/wallet/{WALLET}"),
                auth,
            )?;

            // Mine past coinbase maturity so the wallet has spendable funds.
            let address = new_address(&wallet_rpc)?;
            wallet_rpc.generate_to_address(101, &address)?;
            anyhow::Ok(wallet_rpc)
        };

        match setup.await {
            Ok(wallet_rpc) => Ok(Bitcoind {
                container,
                name,
                wallet_rpc,
            }),
            Err(e) => {
                dump_logs("bitcoind", &container).await;
                Err(e)
            },
        }
    }

    pub fn container_name(&self) -> &str {
        &self.name
    }

    pub fn container(&self) -> &ContainerAsync<GenericImage> {
        &self.container
    }

    /// Mines `blocks` blocks to the harness wallet.
    pub fn mine(&self, blocks: u64) -> anyhow::Result<()> {
        let address = new_address(&self.wallet_rpc)?;
        self.wallet_rpc.generate_to_address(blocks, &address)?;
        Ok(())
    }

    /// Sends `amount_sat` to each of the given addresses and mines a block to confirm.
    pub fn fund(&self, addresses: &[String], amount_sat: u64) -> anyhow::Result<()> {
        for addr in addresses {
            let addr = Address::from_str(addr)
                .with_context(|| format!("parsing funding address {addr}"))?
                .assume_checked();
            self.wallet_rpc.send_to_address(
                &addr,
                bitcoin::Amount::from_sat(amount_sat),
                None,
                None,
                None,
                None,
                None,
                None,
            )?;
        }
        self.mine(1)
    }
}

fn new_address(rpc: &Client) -> anyhow::Result<Address> {
    Ok(rpc.get_new_address(None, None)?.assume_checked())
}
