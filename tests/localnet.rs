//! End-to-end agent wallet checks against a local validator.
//!
//! Run with a fresh `solana-test-validator` on :8899:
//!   cargo test --test localnet -- --ignored --test-threads=1

use manus_agent_wallet::{
    approval::NoApprover,
    budget::{Budget, Limits},
    engine::{AgentWallet, Request},
    gbrain::Gbrain,
    store::Store,
};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use std::sync::Arc;

const RPC: &str = "http://127.0.0.1:8899";

fn rpc() -> RpcClient {
    RpcClient::new_with_commitment(RPC.to_string(), CommitmentConfig::confirmed())
}

async fn fund(rpc: &RpcClient, to: &Pubkey, lamports: u64) {
    let signature = rpc.request_airdrop(to, lamports).await.expect("airdrop");
    for _ in 0..60 {
        if rpc.confirm_transaction(&signature).await.unwrap_or(false) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("airdrop did not confirm");
}

/// Create a 6-decimal mint and give `owner` 1,000 tokens.
async fn mint_tokens(rpc: &RpcClient, authority: &Keypair, owner: &Pubkey) -> Pubkey {
    let mint = Keypair::new();
    let rent = rpc
        .get_minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN)
        .await
        .unwrap();
    let owner_ata = get_associated_token_address(owner, &mint.pubkey());
    let instructions = vec![
        solana_system_interface::instruction::create_account(
            &authority.pubkey(),
            &mint.pubkey(),
            rent,
            spl_token::state::Mint::LEN as u64,
            &spl_token::id(),
        ),
        spl_token::instruction::initialize_mint2(
            &spl_token::id(),
            &mint.pubkey(),
            &authority.pubkey(),
            None,
            6,
        )
        .unwrap(),
        create_associated_token_account(
            &authority.pubkey(),
            owner,
            &mint.pubkey(),
            &spl_token::id(),
        ),
        spl_token::instruction::mint_to(
            &spl_token::id(),
            &mint.pubkey(),
            &owner_ata,
            &authority.pubkey(),
            &[],
            1_000_000_000,
        )
        .unwrap(),
    ];
    let blockhash = rpc.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&authority.pubkey()),
        &[authority, &mint],
        blockhash,
    );
    rpc.send_and_confirm_transaction(&tx)
        .await
        .expect("create mint");
    mint.pubkey()
}

async fn wallet_with(budget: Budget) -> (Arc<AgentWallet>, std::path::PathBuf) {
    let signer = Keypair::new();
    fund(&rpc(), &signer.pubkey(), 5_000_000_000).await;
    let path = std::env::temp_dir().join(format!("agent-localnet-{}.db", uuid::Uuid::new_v4()));
    let store = Store::open(&path).await.unwrap();
    store.save_budget(&budget).await.unwrap();
    let wallet = AgentWallet::new(
        rpc(),
        signer,
        "localnet".into(),
        store,
        Arc::new(NoApprover),
        Gbrain::disabled(),
    );
    (Arc::new(wallet), path)
}

fn status(result: &serde_json::Value) -> String {
    result["operation"]["status"]
        .as_str()
        .unwrap_or("?")
        .to_string()
}

#[tokio::test]
#[ignore]
async fn token_transfers_respect_token_limits() {
    let rpc = rpc();
    let authority = Keypair::new();
    fund(&rpc, &authority.pubkey(), 2_000_000_000).await;

    let mut budget = Budget::default_for_cluster("localnet");
    let (wallet, db) = wallet_with(budget.clone()).await;
    let mint = mint_tokens(&rpc, &authority, &wallet.address()).await;
    budget.send_tokens.insert(
        mint.to_string(),
        Limits {
            per_op: 10_000_000,
            daily: 15_000_000,
        },
    );
    wallet.store.save_budget(&budget).await.unwrap();

    let recipient = Pubkey::new_unique();
    let send = |amount: &str| Request::Send {
        to: recipient.to_string(),
        amount: amount.into(),
        token: mint.to_string(),
        memo: None,
    };

    let first = wallet
        .execute(send("10"), Some("tok-1".into()), false)
        .await
        .unwrap();
    assert!(
        matches!(status(&first).as_str(), "confirmed" | "finalized"),
        "{first}"
    );
    let recipient_ata = get_associated_token_address(&recipient, &mint);
    let balance = rpc.get_token_account_balance(&recipient_ata).await.unwrap();
    assert_eq!(balance.amount, "10000000");

    // 10 + 10 would pass the 15 daily token limit.
    let second = wallet
        .execute(send("10"), Some("tok-2".into()), false)
        .await
        .unwrap();
    assert_eq!(status(&second), "denied", "{second}");

    // Tokens without a budget are never automatic.
    let other_mint = mint_tokens(&rpc, &authority, &wallet.address()).await;
    let unbudgeted = Request::Send {
        to: recipient.to_string(),
        amount: "1".into(),
        token: other_mint.to_string(),
        memo: None,
    };
    let third = wallet
        .execute(unbudgeted, Some("tok-3".into()), false)
        .await
        .unwrap();
    assert_eq!(status(&third), "denied", "{third}");

    let balance = rpc.get_token_account_balance(&recipient_ata).await.unwrap();
    assert_eq!(balance.amount, "10000000");
    let _ = std::fs::remove_file(db);
}

#[tokio::test]
#[ignore]
async fn concurrent_sends_cannot_jointly_exceed_the_daily_limit() {
    let mut budget = Budget::default_for_cluster("localnet");
    budget.send_sol = Limits {
        per_op: 400_000_000,
        daily: 1_000_000_000,
    };
    let (wallet, db) = wallet_with(budget).await;
    let recipient = Pubkey::new_unique();

    let mut tasks = Vec::new();
    for i in 0..5 {
        let wallet = wallet.clone();
        tasks.push(tokio::spawn(async move {
            let request = Request::Send {
                to: recipient.to_string(),
                amount: "0.3".into(),
                token: "SOL".into(),
                memo: None,
            };
            wallet
                .execute(request, Some(format!("par-{i}")), false)
                .await
                .unwrap()
        }));
    }
    let mut landed = 0;
    for task in tasks {
        let result = task.await.unwrap();
        if matches!(status(&result).as_str(), "confirmed" | "finalized") {
            landed += 1;
        }
    }
    assert_eq!(landed, 3, "0.3 × 3 fits in 1 SOL; a fourth must be refused");
    let received = rpc().get_balance(&recipient).await.unwrap();
    assert_eq!(received, 900_000_000);
    let _ = std::fs::remove_file(db);
}
