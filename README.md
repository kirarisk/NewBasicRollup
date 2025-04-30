# Solana Rust Client Extension

This crate provides extensions for the Solana Rust client, focusing on compute unit estimation and optimization.

## Features
* Estimates compute units for Solana transactions
* Optimizes compute unit usage by adding a compute budget instruction

## Usage

To use this crate, add it to your `Cargo.toml` file:

```toml
[dependencies]
solana-client-ext = { git = "https://github.com/bergabman/Solana-Rust-Client-Extension.git", version ="0.1.0"} # Replace with the right version
```

```rust
use solana_client::rpc_client::RpcClient;
use solana_client_ext::RpcClientExt;
use solana_sdk::{
    message::Message, signature::read_keypair_file, signer::Signer, system_instruction,
    transaction::Transaction,
};
fn main() {
    let rpc_client = RpcClient::new("https://api.devnet.solana.com");
    let keypair = read_keypair_file("~/.config/solana/id.json").unwrap();
    let keypair2 = read_keypair_file("~/.config/solana/_id.json").unwrap();
    let created_ix = system_instruction::transfer(&keypair.pubkey(), &keypair2.pubkey(), 10000);
    let mut msg = Message::new(&[created_ix], Some(&keypair.pubkey()));

    let optimized_cu = rpc_client
        .optimize_compute_units_msg(&mut msg, &[&keypair])
        .unwrap();
    println!("optimized cu {}", optimized_cu);

    let tx = Transaction::new(&[keypair], msg, rpc_client.get_latest_blockhash().unwrap());
    let result = rpc_client
        .send_and_confirm_transaction_with_spinner(&tx)
        .unwrap();

    println!(
        "sig https://explorer.solana.com/tx/{}?cluster=devnet",
        result
    );
}
```
[tx](img/optimized_compute.png)