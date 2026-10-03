//! End-to-end agent wallet checks against a local validator.
//!
//! Run with a fresh `solana-test-validator` on :8899:
//!   cargo test --test localnet -- --ignored --test-threads=1

use manus_agent_wallet::{
    approval::{ApprovalOutcome, Approver, NoApprover},
    budget::{Budget, Category, Exposure, Limits},
    engine::{AgentWallet, Request},
    gbrain::Gbrain,
    invoice, mcp,
    store::Store,
    tx,
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

/// Approves after a pause, standing in for a human reaching for Touch ID.
struct SlowApprover(std::time::Duration);

impl Approver for SlowApprover {
    fn request(&self, _reason: &str) -> ApprovalOutcome {
        std::thread::sleep(self.0);
        ApprovalOutcome::Approved {
            method: "slow-test".into(),
        }
    }

    fn describe(&self) -> &'static str {
        "slow test approver"
    }
}

async fn wallet_with(budget: Budget) -> (Arc<AgentWallet>, std::path::PathBuf) {
    wallet_with_approver(budget, Arc::new(NoApprover)).await
}

async fn wallet_with_approver(
    budget: Budget,
    approver: Arc<dyn Approver>,
) -> (Arc<AgentWallet>, std::path::PathBuf) {
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
        approver,
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

    let recipient = Keypair::new().pubkey();
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
    let recipient = Keypair::new().pubkey();

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

#[tokio::test]
#[ignore]
async fn independent_wallet_instances_share_the_daily_limit() {
    let mut budget = Budget::default_for_cluster("localnet");
    budget.send_sol = Limits {
        per_op: 400_000_000,
        daily: 1_000_000_000,
    };
    let (first, db) = wallet_with(budget).await;
    let second = Arc::new(AgentWallet::new(
        rpc(),
        Keypair::try_from(first.signer.to_bytes().as_slice()).unwrap(),
        "localnet".into(),
        Store::open(&db).await.unwrap(),
        Arc::new(NoApprover),
        Gbrain::disabled(),
    ));
    let recipient = Keypair::new().pubkey();
    let mut tasks = Vec::new();
    for i in 0..5 {
        let wallet = if i % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        tasks.push(tokio::spawn(async move {
            wallet
                .execute(
                    Request::Send {
                        to: recipient.to_string(),
                        amount: "0.3".into(),
                        token: "SOL".into(),
                        memo: None,
                    },
                    Some(format!("multi-{i}")),
                    false,
                )
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
    assert_eq!(
        landed, 3,
        "two wallet instances must reserve the same allowance"
    );
    assert_eq!(rpc().get_balance(&recipient).await.unwrap(), 900_000_000);
    let _ = std::fs::remove_file(db.with_extension("spend.lock"));
    let _ = std::fs::remove_file(db);
}

#[tokio::test]
#[ignore]
async fn submitting_payment_is_recovered_after_reopening_the_wallet() {
    let (first, db) = wallet_with(Budget::default_for_cluster("localnet")).await;
    let recipient = Keypair::new().pubkey();
    let transaction = tx::sign_instructions(
        &first.rpc,
        &first.signer,
        &tx::sol_transfer_instructions(&first.address(), &recipient, 200_000_000, None),
    )
    .await
    .unwrap();
    let signature = transaction.signatures[0].to_string();
    first
        .store
        .insert(
            "recovered",
            None,
            "hash",
            "send",
            "0.2 SOL",
            &serde_json::json!({}),
        )
        .await
        .unwrap();
    first
        .store
        .set_evaluation(
            "recovered",
            "0.2 SOL",
            &Exposure {
                category: Category::Send,
                sol_out: 200_000_000,
                fees: 5_000,
                token_out: None,
                recipient: Some(recipient.to_string()),
            },
            &serde_json::json!({"decision": "auto"}),
        )
        .await
        .unwrap();
    first
        .store
        .set_submitting(
            "recovered",
            &signature,
            &transaction.message.recent_blockhash().to_string(),
        )
        .await
        .unwrap();
    first.rpc.send_transaction(&transaction).await.unwrap();
    let reopened = AgentWallet::new(
        rpc(),
        Keypair::try_from(first.signer.to_bytes().as_slice()).unwrap(),
        "localnet".into(),
        Store::open(&db).await.unwrap(),
        Arc::new(NoApprover),
        Gbrain::disabled(),
    );
    for _ in 0..30 {
        reopened.status().await.unwrap();
        let record = reopened.store.get("recovered").await.unwrap().unwrap();
        if matches!(record.status.as_str(), "confirmed" | "finalized") {
            assert_eq!(rpc().get_balance(&recipient).await.unwrap(), 200_000_000);
            let _ = std::fs::remove_file(db.with_extension("spend.lock"));
            let _ = std::fs::remove_file(db);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("submitting payment did not reconcile after reopening");
}

#[tokio::test]
#[ignore]
async fn missing_signature_after_blockhash_expiry_stays_unknown() {
    let (wallet, db) = wallet_with(Budget::default_for_cluster("localnet")).await;
    wallet
        .store
        .insert(
            "unknown",
            None,
            "hash",
            "send",
            "unknown",
            &serde_json::json!({}),
        )
        .await
        .unwrap();
    wallet
        .store
        .set_evaluation(
            "unknown",
            "unknown",
            &Exposure {
                category: Category::Send,
                sol_out: 200_000_000,
                fees: 5_000,
                token_out: None,
                recipient: None,
            },
            &serde_json::json!({"decision": "auto"}),
        )
        .await
        .unwrap();
    let signature = Keypair::new().sign_message(b"never submitted").to_string();
    wallet
        .store
        .set_submitting(
            "unknown",
            &signature,
            &solana_sdk::hash::Hash::default().to_string(),
        )
        .await
        .unwrap();
    wallet.status().await.unwrap();
    assert_eq!(
        wallet.store.get("unknown").await.unwrap().unwrap().status,
        "submission_unknown"
    );
    assert_eq!(
        wallet.store.spent_last_day(None).await.unwrap().send_sol,
        200_000_000
    );
    let invoice = invoice::Invoice {
        invoice_id: "inv_unknown_after_expiry".into(),
        payee: wallet.address().to_string(),
        token: "SOL".into(),
        amount: "0.2".into(),
        description: "unknown settlement".into(),
        expires_at: "2000-01-01T00:00:00Z".into(),
        cluster: "localnet".into(),
    };
    let signed = invoice::sign(&invoice, &wallet.signer).unwrap();
    wallet
        .store
        .put_invoice(
            &invoice.invoice_id,
            "issued",
            &serde_json::to_string(&signed).unwrap(),
        )
        .await
        .unwrap();
    wallet
        .store
        .update_invoice(
            &invoice.invoice_id,
            "confirming",
            Some(&signature),
            Some("unknown"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        wallet.invoice_status(&invoice.invoice_id).await.unwrap()["status"],
        "unknown"
    );
    let _ = std::fs::remove_file(db.with_extension("spend.lock"));
    let _ = std::fs::remove_file(db);
}

#[tokio::test]
#[ignore]
async fn invoice_payment_is_found_beyond_the_first_history_page() {
    let (payee, db) = wallet_with(Budget::default_for_cluster("localnet")).await;
    let payer = Keypair::new();
    fund(&rpc(), &payer.pubkey(), 2_000_000_000).await;
    let created = payee
        .create_invoice("SOL", "0.1", "pagination test", 168)
        .await
        .unwrap();
    let invoice_id = created["invoice"]["invoice_id"].as_str().unwrap();
    let payment = tx::sign_instructions(
        &rpc(),
        &payer,
        &tx::sol_transfer_instructions(
            &payer.pubkey(),
            &payee.address(),
            100_000_000,
            Some(invoice_id),
        ),
    )
    .await
    .unwrap();
    rpc().send_and_confirm_transaction(&payment).await.unwrap();

    for index in 0..55 {
        let memo = format!("unrelated-{index}");
        let noise = tx::sign_instructions(
            &rpc(),
            &payer,
            &tx::sol_transfer_instructions(&payer.pubkey(), &payee.address(), 1, Some(&memo)),
        )
        .await
        .unwrap();
        rpc().send_and_confirm_transaction(&noise).await.unwrap();
    }

    for _ in 0..30 {
        let status = payee.invoice_status(invoice_id).await.unwrap();
        if status["status"] == "paid" {
            let _ = std::fs::remove_file(db.with_extension("spend.lock"));
            let _ = std::fs::remove_file(db);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("payment beyond the first history page was not finalized and found");
}

#[tokio::test]
#[ignore]
async fn two_agents_complete_invoice_through_mcp_tools() {
    let (payee, payee_db) = wallet_with(Budget::default_for_cluster("localnet")).await;
    let (payer, payer_db) = wallet_with_approver(
        Budget::default_for_cluster("localnet"),
        Arc::new(SlowApprover(std::time::Duration::ZERO)),
    )
    .await;

    let created = mcp::call(
        &payee,
        "create_invoice",
        &serde_json::json!({
            "amount": "0.1",
            "token": "SOL",
            "description": "localnet pilot",
        }),
    )
    .await
    .unwrap();
    let invoice_id = created["invoice"]["invoice_id"].as_str().unwrap();
    let paid = mcp::call(
        &payer,
        "pay_invoice",
        &serde_json::json!({"invoice": created}),
    )
    .await
    .unwrap();
    assert_eq!(paid["counterparty_trusted"], true, "{paid}");
    let signature = paid["payment"]["operation"]["signature"]
        .as_str()
        .expect("payment receipt has a signature");

    let replay = mcp::call(
        &payer,
        "pay_invoice",
        &serde_json::json!({"invoice": created}),
    )
    .await
    .unwrap();
    let replay_signature = replay["payment"]["signature"]
        .as_str()
        .or_else(|| replay["payment"]["operation"]["signature"].as_str());
    assert_eq!(replay_signature, Some(signature), "{replay}");

    // A confirmed send can return before the validator accumulates the slots
    // needed for finality. Allow up to 30 seconds for that separate condition.
    for _ in 0..120 {
        let state = mcp::call(
            &payee,
            "invoice_status",
            &serde_json::json!({"invoice_id": invoice_id}),
        )
        .await
        .unwrap();
        if state["status"] == "paid" {
            assert_eq!(state["payment"]["signature"], signature);
            for db in [payee_db, payer_db] {
                let _ = std::fs::remove_file(db.with_extension("spend.lock"));
                let _ = std::fs::remove_file(db);
            }
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("payee never observed a finalized invoice payment");
}

#[tokio::test]
#[ignore]
async fn waiting_for_approval_does_not_block_other_spending() {
    let mut budget = Budget::default_for_cluster("localnet");
    budget.send_sol = Limits {
        per_op: 200_000_000,
        daily: 1_000_000_000,
    };
    let (wallet, db) = wallet_with_approver(
        budget,
        Arc::new(SlowApprover(std::time::Duration::from_secs(8))),
    )
    .await;
    let recipient = Keypair::new().pubkey();
    let send = |amount: &str| Request::Send {
        to: recipient.to_string(),
        amount: amount.into(),
        token: "SOL".into(),
        memo: None,
    };

    let big_wallet = wallet.clone();
    let big_request = send("0.5");
    let big = tokio::spawn(async move {
        big_wallet
            .execute(big_request, Some("big-approval".into()), false)
            .await
            .unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    // An in-budget payment goes through while the human is still deciding.
    let started = std::time::Instant::now();
    let small = wallet
        .execute(send("0.1"), Some("small-auto".into()), false)
        .await
        .unwrap();
    assert!(
        matches!(status(&small).as_str(), "confirmed" | "finalized"),
        "{small}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
    assert!(
        !big.is_finished(),
        "the big payment should still be waiting"
    );

    // The pending 0.5 is reserved: 0.5 + 0.1 + 0.45 would pass the 1 SOL day.
    let preview = wallet.execute(send("0.45"), None, true).await.unwrap();
    assert_eq!(
        preview["decision"]["decision"], "needs_approval",
        "{preview}"
    );
    // Status stays responsive too.
    wallet.status().await.unwrap();

    let big = big.await.unwrap();
    assert!(
        matches!(status(&big).as_str(), "confirmed" | "finalized"),
        "{big}"
    );
    assert_eq!(big["operation"]["approved_by"], "slow-test");
    let received = rpc().get_balance(&recipient).await.unwrap();
    assert_eq!(received, 600_000_000);
    let _ = std::fs::remove_file(db);
}
