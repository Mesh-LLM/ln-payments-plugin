use super::*;
use async_trait::async_trait;
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use lightning_invoice::{Currency, InvoiceBuilder, PaymentHash, PaymentSecret};
use mesh_llm_payments::ledger::{ApprovalMode, Policy};
use mesh_llm_payments_types::control::DEFAULT_RATE_CEILING;
use mesh_llm_wallet::provider::*;
use std::sync::{
    Mutex,
    atomic::{AtomicU8, AtomicUsize, Ordering},
};

#[derive(Default)]
struct Network {
    next: AtomicU8,
    received: Mutex<BTreeMap<String, Transaction>>,
    sends: AtomicUsize,
    balances: AtomicUsize,
    slow_lookup: Mutex<Option<String>>,
    fail_next_pay: std::sync::atomic::AtomicBool,
}
struct Wallet(Arc<Network>, bool);
pub(crate) fn invoice(number: u8, amount: u64) -> Invoice {
    let secret = SecretKey::from_slice(&[7; 32]).unwrap();
    let raw = InvoiceBuilder::new(Currency::Bitcoin)
        .description("offline fixture".into())
        .payment_hash(PaymentHash([number; 32]))
        .payment_secret(PaymentSecret([42; 32]))
        .current_timestamp()
        .expiry_time(std::time::Duration::from_secs(3600))
        .min_final_cltv_expiry_delta(144)
        .amount_milli_satoshis(amount)
        .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &secret))
        .unwrap()
        .to_string();
    Invoice::parse(&raw).unwrap()
}
#[async_trait]
impl WalletProvider for Wallet {
    async fn balance(&self) -> Result<Balance> {
        self.0.balances.fetch_add(1, Ordering::SeqCst);
        Ok(Balance {
            spendable_msat: 1_000_000,
        })
    }
    async fn transactions(&self, _: usize) -> Result<Vec<Transaction>> {
        Ok(vec![])
    }
    async fn create_invoice(&self, amount: Option<u64>, _: u32) -> Result<Invoice> {
        Ok(invoice(
            self.0.next.fetch_add(1, Ordering::SeqCst) + 1,
            amount.unwrap(),
        ))
    }
    async fn pay(
        &self,
        invoice: &Invoice,
        amount: u64,
        _: u64,
    ) -> std::result::Result<Transaction, PayError> {
        self.0.sends.fetch_add(1, Ordering::SeqCst);
        if self.0.fail_next_pay.swap(false, Ordering::SeqCst) {
            // A brand-new seller wallet the buyer cannot route to yet.
            return Ok(Transaction {
                id: invoice.payment_hash.clone(),
                payment_hash: Some(invoice.payment_hash.clone()),
                inbound: false,
                amount_msat: amount,
                fee_msat: 0,
                status: PaymentStatus::Failed,
                claiming: false,
                status_msg: Some("could not find usable route to send payment over".into()),
                created_at_ms: mesh_llm_wallet::now_ms(),
                settled_at_ms: Some(mesh_llm_wallet::now_ms()),
            });
        }
        let payment = Transaction {
            id: invoice.payment_hash.clone(),
            payment_hash: Some(invoice.payment_hash.clone()),
            inbound: false,
            amount_msat: amount,
            fee_msat: 0,
            status: PaymentStatus::Succeeded,
            claiming: false,
            status_msg: None,
            created_at_ms: mesh_llm_wallet::now_ms(),
            settled_at_ms: Some(mesh_llm_wallet::now_ms()),
        };
        let mut incoming = payment.clone();
        incoming.inbound = true;
        self.0
            .received
            .lock()
            .unwrap()
            .insert(invoice.payment_hash.clone(), incoming);
        Ok(payment)
    }
    async fn lookup(&self, hash: &str) -> Result<Option<Transaction>> {
        let slow = self.0.slow_lookup.lock().unwrap().as_deref() == Some(hash);
        if slow {
            std::future::pending::<()>().await;
        }
        Ok(self
            .0
            .received
            .lock()
            .unwrap()
            .get(hash)
            .cloned()
            .map(|mut payment| {
                payment.inbound = self.1;
                payment
            }))
    }
    async fn wait_for_payment(&self, hash: &str) -> Result<Transaction> {
        self.lookup(hash)
            .await?
            .context("fixture payment not received")
    }
}
struct Fixture {
    seller: Arc<Engine>,
    buyer: Engine,
    network: Arc<Network>,
    _seller_dir: tempfile::TempDir,
    _buyer_dir: tempfile::TempDir,
}
/// Rates at the shipped default ceiling, or a modest price when none ships.
fn within_default_ceiling() -> Pricing {
    let (input, output) = DEFAULT_RATE_CEILING.unwrap_or((500_000, 1_500_000));
    Pricing {
        input_msat_per_million: input,
        output_msat_per_million: output,
        cache_discount_percent: 0,
    }
}
fn price() -> Pricing {
    Pricing {
        input_msat_per_million: 1_000_000,
        output_msat_per_million: 1_000_000,
        cache_discount_percent: 50,
    }
}
impl Fixture {
    fn new() -> Self {
        let network = Arc::new(Network::default());
        let seller_dir = tempfile::tempdir().unwrap();
        let buyer_dir = tempfile::tempdir().unwrap();
        let seller_service = Arc::new(
            PaymentService::with_provider(
                seller_dir.path(),
                Arc::new(Wallet(network.clone(), true)),
            )
            .unwrap(),
        );
        seller_service
            .ledger
            .set_pricing("model", Some(&price()))
            .unwrap();
        let buyer_service = Arc::new(
            PaymentService::with_provider(
                buyer_dir.path(),
                Arc::new(Wallet(network.clone(), false)),
            )
            .unwrap(),
        );
        buyer_service
            .ledger
            .set_policy(&Policy {
                mode: ApprovalMode::Automatic,
                daily_budget_msat: Some(100_000),
                ..Default::default()
            })
            .unwrap();
        let seller = Arc::new(Engine::new(seller_service, "706179", 0, seller_dir.path()).unwrap());
        let mut buyer = Engine::new(buyer_service, "706179", 100_000, buyer_dir.path()).unwrap();
        buyer.buying.insert("model".into(), price());
        buyer.buy_peers.insert("seller".into());
        Self {
            seller,
            buyer,
            network,
            _seller_dir: seller_dir,
            _buyer_dir: buyer_dir,
        }
    }
    async fn prepare(&self) -> String {
        let admission = self
            .seller
            .channel("buyer", Message::Quote)
            .await
            .unwrap()
            .remove(0);
        let Message::Admission { terms, .. } = &admission else {
            panic!("not admission")
        };
        let id = terms.id.clone();
        let paid = self
            .buyer
            .channel("seller", admission)
            .await
            .unwrap()
            .remove(0);
        self.seller.channel("buyer", paid).await.unwrap();
        id
    }
}
fn gate(exchange: &str, peer: &str) -> Value {
    json!({"phase":"request_received","observation_point":"serving_host_ingress","model":"model","requested_by_node_id":peer,"exchange_id":exchange})
}
fn terminal(exchange: &str, usage: Value) -> Value {
    json!({"phase":"exchange_finished","ingress_observation_point":"serving_host_ingress","exchange_id":exchange,"execution_outcome":"success","status":200,"usage":usage})
}

#[tokio::test]
async fn cold_deposit_gate_final_recovery_and_warm_admission() {
    let f = Fixture::new();
    assert_eq!(
        f.seller.gate(&gate("cold", "buyer")).unwrap().status,
        Some(402)
    );
    let id = f.prepare().await;
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.seller.gate(&gate("x", "attacker")).unwrap().status,
        Some(402)
    );
    assert_eq!(
        f.seller.gate(&gate("x", "buyer")).unwrap().decision,
        Decision::Allow
    );
    assert_eq!(
        f.seller.gate(&gate("y", "buyer")).unwrap().status,
        Some(402)
    );
    let (_,settlement)=f.seller.terminal(&terminal("x",json!({"prompt_tokens":2000,"completion_tokens":1000,"prompt_tokens_details":{"cached_tokens":1000}}))).await.unwrap().unwrap();
    let Message::Settlement { bill, .. } = &settlement else {
        panic!("not bill")
    };
    assert_eq!(bill.bill_msat, 2500);
    assert_eq!(bill.remaining_msat, 1500);
    assert!(
        f.seller
            .channel("attacker", Message::Recover { id: id.clone() })
            .await
            .is_err()
    );
    let recovered = f
        .seller
        .channel("buyer", Message::Recover { id: id.clone() })
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        serde_json::to_value(&settlement).unwrap(),
        serde_json::to_value(&recovered).unwrap()
    );
    let paid = f
        .buyer
        .channel("seller", settlement)
        .await
        .unwrap()
        .remove(0);
    let ready = f.seller.channel("buyer", paid).await.unwrap();
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 2);
    assert!(f.seller.service.metered_trusted("buyer").await.unwrap());
    let warm = ready
        .into_iter()
        .find(|m| matches!(m, Message::Admission { .. }))
        .unwrap();
    let Message::Admission { terms, invoice } = &warm else {
        unreachable!()
    };
    assert_eq!(terms.deposit_msat, 0);
    assert!(invoice.is_none());
    let paid = f.buyer.channel("seller", warm).await.unwrap().remove(0);
    f.seller.channel("buyer", paid).await.unwrap();
    assert_eq!(
        f.seller.gate(&gate("warm", "buyer")).unwrap().decision,
        Decision::Allow
    );
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn absent_usage_abandons_without_bill_or_strike() {
    let f = Fixture::new();
    let id = f.prepare().await;
    f.seller.gate(&gate("x", "buyer")).unwrap();
    let (_, message) = f
        .seller
        .terminal(&terminal("x", Value::Null))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(message, Message::Abandoned { .. }));
    assert_eq!(
        f.seller
            .service
            .ledger
            .metered_unpaid_finals("buyer")
            .unwrap(),
        0
    );
    assert!(matches!(
        f.seller
            .channel("buyer", Message::Recover { id })
            .await
            .unwrap()[0],
        Message::Abandoned { .. }
    ));
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn unsolicited_seller_or_excess_rates_never_pay() {
    let f = Fixture::new();
    let admission = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0);
    assert!(f.buyer.channel("attacker", admission).await.is_err());
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn forged_final_bill_and_wrong_sender_are_rejected() {
    let f = Fixture::new();
    let id = f.prepare().await;
    f.seller.gate(&gate("x", "buyer")).unwrap();
    let (_, message) = f
        .seller
        .terminal(&terminal(
            "x",
            json!({"prompt_tokens":2000,"completion_tokens":1000}),
        ))
        .await
        .unwrap()
        .unwrap();
    let Message::Settlement { mut bill, .. } = message else {
        unreachable!()
    };
    bill.bill_msat += 1;
    assert!(
        f.buyer
            .channel("seller", Message::Settlement { id, bill })
            .await
            .is_err()
    );
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 1);
}
#[test]
fn local_gateway_and_unpriced_models_are_not_gated() {
    let f = Fixture::new();
    let mut event = gate("x", "buyer");
    event["observation_point"] = json!("gateway_ingress");
    assert_eq!(f.seller.gate(&event).unwrap().decision, Decision::Abstain);
    event = gate("x", "buyer");
    event["model"] = json!("free");
    assert_eq!(f.seller.gate(&event).unwrap().decision, Decision::Abstain);
}

#[test]
fn normalized_cached_usage_and_partial_usage_are_not_confused() {
    let event = terminal(
        "x",
        json!({"prompt_tokens":10,"completion_tokens":5,"cached_prompt_tokens":6}),
    );
    assert_eq!(reported_usage(&event).unwrap().cached_tokens, 6);
    assert!(reported_usage(&terminal("x", json!({"prompt_tokens":10}))).is_none());
    assert!(
        reported_usage(&terminal(
            "x",
            json!({"prompt_tokens":10,"completion_tokens":5,"cached_prompt_tokens":11})
        ))
        .is_none()
    );
}
#[tokio::test]
async fn duplicate_admission_and_paid_replay_do_not_buy_or_dispatch_twice() {
    let f = Fixture::new();
    let admission = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0);
    let duplicate: Message =
        serde_json::from_value(serde_json::to_value(&admission).unwrap()).unwrap();
    let paid = f
        .buyer
        .channel("seller", admission)
        .await
        .unwrap()
        .remove(0);
    let Message::Paid { id } = &paid else {
        unreachable!()
    };
    let id = id.clone();
    f.seller.channel("buyer", paid).await.unwrap();
    f.buyer.channel("seller", duplicate).await.unwrap();
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 1);
    f.seller.gate(&gate("x", "buyer")).unwrap();
    f.seller
        .channel("buyer", Message::Paid { id })
        .await
        .unwrap();
    assert_eq!(
        f.seller.gate(&gate("y", "buyer")).unwrap().status,
        Some(402)
    );
}
#[tokio::test]
async fn rates_above_local_ceiling_do_not_pay() {
    let f = Fixture::new();
    let Message::Admission { mut terms, invoice } = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0)
    else {
        unreachable!()
    };
    terms.pricing.output_msat_per_million += 1;
    assert!(
        f.buyer
            .channel("seller", Message::Admission { terms, invoice })
            .await
            .is_err()
    );
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 0);
}

/// Built-in payments paid any seller under `policy automatic` within the
/// daily budget, with no per-seller or per-model configuration. The plugin
/// keeps that: an unconfigured buyer pays a seller it quoted, is capped by the
/// budget, refuses an unsolicited admission from a peer it never quoted, and
/// buys nothing under the default free-only policy.
#[tokio::test]
async fn unconfigured_buyer_pays_under_automatic_policy_only_sellers_it_quoted() {
    let mut f = Fixture::new();
    f.buyer.buying.clear();
    f.buyer.buy_peers.clear();
    f.buyer.cap_msat = 0;
    // Within the shipped default rate ceiling, which an unconfigured buyer keeps.
    f.seller
        .service
        .ledger
        .set_pricing("model", Some(&within_default_ceiling()))
        .unwrap();
    assert!(f.buyer.buyer_enabled("seller").await);
    // No handshake with this seller yet: its admission is refused unpaid.
    let unsolicited = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0);
    assert!(f.buyer.channel("seller", unsolicited).await.is_err());
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 0);
    // Selecting the seller as a backend starts the handshake; its quote is paid.
    let hello = f.buyer.peer_up("seller").await.unwrap();
    assert!(matches!(hello.first(), Some(Message::Hello { .. })));
    let admission = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0);
    let Message::Admission { terms, .. } = &admission else {
        unreachable!()
    };
    let id = terms.id.clone();
    let paid = f
        .buyer
        .channel("seller", admission)
        .await
        .unwrap()
        .remove(0);
    assert!(matches!(paid, Message::Paid { .. }));
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 1);
    // The admission is capped by the policy budget, not a static cap.
    let terms = f.buyer.service.ledger.request_terms(&id).unwrap().unwrap();
    assert_eq!(terms.max_total_msat, 100_000);
    // Free-only policy buys nothing, from anyone.
    f.buyer
        .service
        .ledger
        .set_policy(&Policy {
            mode: ApprovalMode::FreeOnly,
            daily_budget_msat: None,
            ..Default::default()
        })
        .unwrap();
    assert!(!f.buyer.buyer_enabled("seller").await);
    assert!(f.buyer.peer_up("other").await.unwrap().is_empty());
}

#[tokio::test]
async fn repeated_quote_returns_same_unconsumed_invoice() {
    let f = Fixture::new();
    let first = f.seller.channel("buyer", Message::Quote).await.unwrap();
    let second = f.seller.channel("buyer", Message::Quote).await.unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(second).unwrap()
    );
    assert_eq!(f.network.next.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn peer_reconnect_recovers_debt_before_new_quote() {
    let f = Fixture::new();
    let id = f.prepare().await;
    let messages = f.buyer.peer_up("seller").await.unwrap();
    assert!(matches!(&messages[1],Message::Recover{id:recovered} if recovered==&id));
    assert!(matches!(messages.last().unwrap(), Message::Quote));
    assert!(f.buyer.peer_up("attacker").await.unwrap().is_empty());
}

#[tokio::test]
async fn gateway_terminal_cannot_settle_sellers_ticket() {
    let f = Fixture::new();
    f.prepare().await;
    f.seller.gate(&gate("x", "buyer")).unwrap();
    let mut event = terminal("x", json!({"prompt_tokens":9000,"completion_tokens":9000}));
    event["ingress_observation_point"] = json!("gateway_ingress");
    assert!(f.seller.terminal(&event).await.unwrap().is_none());
}

#[tokio::test]
async fn known_buyer_waits_for_inflight_deposit_unknown_buyer_does_not() {
    let f = Fixture::new();
    f.seller
        .channel(
            "buyer",
            Message::Hello {
                version: 1,
                models: vec!["model".into()],
            },
        )
        .await
        .unwrap();
    let event = gate("cold-wait", "buyer");
    let (decision, _) = tokio::join!(
        f.seller
            .gate_wait(&event, std::time::Duration::from_secs(1)),
        f.prepare()
    );
    assert_eq!(decision.unwrap().decision, Decision::Allow);
    assert_eq!(
        f.seller
            .gate_wait(
                &gate("unknown", "stranger"),
                std::time::Duration::from_secs(1)
            )
            .await
            .unwrap()
            .status,
        Some(402)
    );
}
#[tokio::test]
async fn buyer_vouches_only_once_for_ready_ticket() {
    let f = Fixture::new();
    let id = f.prepare().await;
    assert!(f.buyer.vouch_once("attacker", &id).is_err());
    assert!(f.buyer.vouch_once("seller", &id).unwrap());
    assert!(!f.buyer.vouch_once("seller", &id).unwrap());
}

#[tokio::test]
async fn selected_buyer_waits_for_authenticated_ready_and_consumes_it_once() {
    let f = Fixture::new();
    let wait = f
        .buyer
        .buyer_wait("seller", "model", None, std::time::Duration::from_secs(2));
    let deliver = async {
        tokio::task::yield_now().await;
        let admission = f
            .seller
            .channel("buyer", Message::Quote)
            .await
            .unwrap()
            .remove(0);
        let paid = f
            .buyer
            .channel("seller", admission)
            .await
            .unwrap()
            .remove(0);
        let ready = f.seller.channel("buyer", paid).await.unwrap().remove(0);
        let Message::Ready { id } = &ready else {
            panic!("missing ready")
        };
        let id = id.clone();
        assert!(
            f.buyer
                .channel("impostor", Message::Ready { id: id.clone() })
                .await
                .is_err()
        );
        f.buyer.channel("seller", ready).await.unwrap();
        id
    };
    let (decision, id) = tokio::join!(wait, deliver);
    assert_ne!(decision.unwrap().decision, Decision::Deny);
    f.buyer
        .channel("seller", Message::Ready { id })
        .await
        .unwrap();
    let denied = f
        .buyer
        .buyer_wait(
            "seller",
            "model",
            None,
            std::time::Duration::from_millis(10),
        )
        .await
        .unwrap();
    assert_eq!(denied.decision, Decision::Deny);
    assert_eq!(denied.status, Some(402));
}

struct StartupFactory {
    opens: AtomicUsize,
    fail: bool,
    network: Arc<Network>,
}
#[async_trait]
impl mesh_llm_wallet::provisioning::WalletFactory for StartupFactory {
    fn is_provisioned(&self, _: &std::path::Path) -> bool {
        false
    }
    async fn open(&self, _: &std::path::Path) -> Result<Arc<dyn WalletProvider>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        ensure!(!self.fail, "offline startup failure");
        Ok(Arc::new(Wallet(self.network.clone(), false)))
    }
}

#[tokio::test]
async fn startup_opens_wallet_once_for_both_roles_without_payment() {
    for cap in [0, 100_000] {
        let directory = tempfile::tempdir().unwrap();
        let factory = Arc::new(StartupFactory {
            opens: AtomicUsize::new(0),
            fail: false,
            network: Arc::new(Network::default()),
        });
        let service =
            Arc::new(PaymentService::with_factory(directory.path(), factory.clone()).unwrap());
        let engine = Engine::new(service, "startup", cap, directory.path()).unwrap();
        assert_eq!(factory.opens.load(Ordering::SeqCst), 0);
        let (first, second) = tokio::join!(engine.start(), engine.start());
        first.unwrap();
        second.unwrap();
        engine.service.wallet().await.unwrap();
        assert_eq!(factory.opens.load(Ordering::SeqCst), 1);
        assert_eq!(factory.network.balances.load(Ordering::SeqCst), 2);
        assert_eq!(factory.network.sends.load(Ordering::SeqCst), 0);
        assert!(engine.service.ledger.requests().unwrap().is_empty());
    }
}

#[tokio::test]
async fn startup_propagates_wallet_failure() {
    let directory = tempfile::tempdir().unwrap();
    let factory = Arc::new(StartupFactory {
        opens: AtomicUsize::new(0),
        fail: true,
        network: Arc::new(Network::default()),
    });
    let service =
        Arc::new(PaymentService::with_factory(directory.path(), factory.clone()).unwrap());
    let engine = Engine::new(service, "startup", 0, directory.path()).unwrap();
    let error = engine.start().await.unwrap_err();
    assert!(format!("{error:#}").contains("offline startup failure"));
    assert_eq!(factory.opens.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_quotes_share_one_unconsumed_admission() {
    let f = Fixture::new();
    let (first, second) = tokio::join!(
        f.seller.channel("buyer", Message::Quote),
        f.seller.channel("buyer", Message::Quote)
    );
    assert_eq!(
        serde_json::to_value(first.unwrap()).unwrap(),
        serde_json::to_value(second.unwrap()).unwrap()
    );
    assert_eq!(f.network.next.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn warm_requests_skip_reconciliation_until_disconnect() {
    let f = Fixture::new();
    assert!(!f.buyer.peer_up("seller").await.unwrap().is_empty());
    assert!(f.buyer.peer_up("seller").await.unwrap().is_empty());
    f.buyer.peer_down("seller");
    assert!(!f.buyer.peer_up("seller").await.unwrap().is_empty());
}

#[tokio::test]
async fn warm_refill_precedes_final_settlement_and_still_obeys_strikes() {
    warm_strike_recovery(false).await;
}

#[tokio::test]
async fn seller_receipt_resumes_ingress_without_buyer_paid_notification() {
    warm_strike_recovery(true).await;
}

async fn warm_strike_recovery(withhold_paid: bool) {
    let f = Fixture::new();
    seed_warm_standing(&f).await;
    let mut withheld = issue_unpaid_finals(&f).await;
    assert_eq!(
        f.seller.gate(&gate("over-limit", "buyer")).unwrap().status,
        Some(402)
    );
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 2);
    let event = gate("resumed", "buyer");
    let resume = f
        .seller
        .gate_wait(&event, std::time::Duration::from_secs(1));
    let settle = settle_while_withholding_notification(&f, withheld.remove(0), withhold_paid);
    let (decision, delayed) = tokio::join!(resume, settle);
    assert_eq!(decision.unwrap().decision, Decision::Allow);
    if let Some(paid) = delayed {
        f.seller.channel("buyer", paid).await.unwrap();
        assert_eq!(
            f.seller
                .service
                .ledger
                .metered_unpaid_finals("buyer")
                .unwrap(),
            mesh_llm_payments::ledger::metered::STRIKE_LIMIT - 1
        );
    }
}

async fn seed_warm_standing(f: &Fixture) {
    f.prepare().await;
    let cold = gate("seed", "buyer");
    assert_eq!(f.seller.gate(&cold).unwrap().decision, Decision::Allow);
    assert!(f.seller.replenish(&cold).await.unwrap().is_some());
    let (_, bill) = f
        .seller
        .terminal(&terminal(
            "seed",
            json!({"prompt_tokens":2000,"completion_tokens":1000}),
        ))
        .await
        .unwrap()
        .unwrap();
    let paid = f.buyer.channel("seller", bill).await.unwrap().remove(0);
    let warm = f
        .seller
        .channel("buyer", paid)
        .await
        .unwrap()
        .into_iter()
        .find(|m| matches!(m, Message::Admission { .. }))
        .unwrap();
    let paid = f.buyer.channel("seller", warm).await.unwrap().remove(0);
    let ready = f.seller.channel("buyer", paid).await.unwrap().remove(0);
    f.buyer.channel("seller", ready).await.unwrap();
}

async fn issue_unpaid_finals(f: &Fixture) -> Vec<Message> {
    // Withhold every final payment: each next admission must arrive before any
    // settlement completes, but the existing three-unpaid-final bound still wins.
    let mut withheld = Vec::new();
    for index in 0..mesh_llm_payments::ledger::metered::STRIKE_LIMIT {
        let event = gate(&format!("warm-{index}"), "buyer");
        assert_eq!(
            f.buyer
                .buyer_wait("seller", "model", None, std::time::Duration::from_secs(1))
                .await
                .unwrap()
                .decision,
            Decision::Abstain
        );
        assert_eq!(f.seller.gate(&event).unwrap().decision, Decision::Allow);
        let (_, mut replies) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            f.seller.replenish(&event),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        let paid = f
            .buyer
            .channel("seller", replies.remove(0))
            .await
            .unwrap()
            .remove(0);
        let ready = f.seller.channel("buyer", paid).await.unwrap().remove(0);
        f.buyer.channel("seller", ready).await.unwrap();
        // Repeated refills reuse the outstanding ticket, not another admission.
        let before = f
            .seller
            .service
            .ledger
            .unpaid_metered_invoices()
            .unwrap()
            .len();
        f.seller.replenish(&event).await.unwrap();
        assert_eq!(
            f.seller
                .service
                .ledger
                .unpaid_metered_invoices()
                .unwrap()
                .len(),
            before
        );
        let (_, bill) = f
            .seller
            .terminal(&terminal(
                &format!("warm-{index}"),
                json!({"prompt_tokens":2000,"completion_tokens":1000}),
            ))
            .await
            .unwrap()
            .unwrap();
        withheld.push(bill);
    }
    withheld
}

async fn settle_while_withholding_notification(
    f: &Fixture,
    bill: Message,
    withhold_paid: bool,
) -> Option<Message> {
    tokio::task::yield_now().await;
    let paid = f.buyer.channel("seller", bill).await.unwrap().remove(0);
    if !withhold_paid {
        f.seller.channel("buyer", paid).await.unwrap();
        return None;
    }
    // Run the actual background observer; no explicit refresh or Paid wakes ingress.
    crate::runtime::observe_receipts(Arc::downgrade(&f.seller));
    Some(paid)
}

#[tokio::test]
async fn seller_restart_observes_receipts_without_a_peer_message() {
    let f = Fixture::new();
    seed_warm_standing(&f).await;
    let mut bills = issue_unpaid_finals(&f).await;
    let paid = f
        .buyer
        .channel("seller", bills.remove(0))
        .await
        .unwrap()
        .remove(0);
    let directory = f._seller_dir.path().to_path_buf();
    let network = f.network.clone();
    drop(f.seller);
    let service = Arc::new(
        PaymentService::with_provider(&directory, Arc::new(Wallet(network, true))).unwrap(),
    );
    let seller = Engine::new(service, "706179", 0, &directory).unwrap();
    assert_eq!(
        seller
            .service
            .ledger
            .metered_unpaid_finals("buyer")
            .unwrap(),
        mesh_llm_payments::ledger::metered::STRIKE_LIMIT
    );
    seller.refresh_receipts().await.unwrap();
    assert_eq!(
        seller
            .service
            .ledger
            .metered_unpaid_finals("buyer")
            .unwrap(),
        mesh_llm_payments::ledger::metered::STRIKE_LIMIT - 1
    );
    seller.channel("buyer", paid).await.unwrap();
    seller.refresh_receipts().await.unwrap();
    assert_eq!(
        seller
            .service
            .ledger
            .metered_unpaid_finals("buyer")
            .unwrap(),
        mesh_llm_payments::ledger::metered::STRIKE_LIMIT - 1
    );
}

#[tokio::test]
async fn slow_receipt_lookup_does_not_delay_another_paid_receipt() {
    let f = Fixture::new();
    seed_warm_standing(&f).await;
    let mut bills = issue_unpaid_finals(&f).await;
    bills.sort_by_key(|message| match message {
        Message::Settlement { bill, .. } => bill.invoice.as_ref().unwrap().payment_hash.clone(),
        _ => panic!("expected settlement invoice"),
    });
    let paid_bill = bills.pop().unwrap();
    let _withheld_paid = f.buyer.channel("seller", paid_bill).await.unwrap();
    let receipts = f.seller.service.ledger.unpaid_metered_invoices().unwrap();
    let paid_hash = f
        .network
        .received
        .lock()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let slow_hash = receipts
        .iter()
        .map(|receipt| &receipt.invoice.payment_hash)
        .filter(|hash| !paid_hash.contains(hash))
        .min()
        .unwrap()
        .clone();
    *f.network.slow_lookup.lock().unwrap() = Some(slow_hash);
    // A blocked lookup may consume its one-second timeout, but an independent
    // paid receipt must release admission before that timeout expires.
    let refresh = f.seller.refresh_receipts();
    let observe = async {
        tokio::time::timeout(std::time::Duration::from_millis(500), async {
            loop {
                let changed = f.seller.changed.notified();
                if f.seller
                    .service
                    .ledger
                    .metered_unpaid_finals("buyer")
                    .unwrap()
                    == mesh_llm_payments::ledger::metered::STRIKE_LIMIT - 1
                {
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("paid receipt was blocked behind a slow lookup");
    };
    let (result, ()) = tokio::join!(refresh, observe);
    assert!(result.is_err(), "the deliberately stuck lookup times out");
    assert_eq!(
        f.seller
            .service
            .ledger
            .metered_unpaid_finals("buyer")
            .unwrap(),
        2
    );
}

/// Run12 cold start: the first deposit to a brand-new seller wallet failed
/// with no route, and every later request waited out the deadline because the
/// seller kept quoting the same dead invoice and the connected buyer never
/// quoted again. A definitively failed deposit must lead to a fresh invoice on
/// the next request, with no restart and no second payment of the dead one.
#[tokio::test]
async fn failed_cold_deposit_quotes_a_fresh_invoice_on_the_next_request() {
    let f = Fixture::new();
    f.network.fail_next_pay.store(true, Ordering::SeqCst);
    // First request: connect and quote.
    let opening = f.buyer.peer_up("seller").await.unwrap();
    assert!(matches!(opening.last().unwrap(), Message::Quote));
    let admission = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0);
    let Message::Admission {
        invoice: Some(dead),
        ..
    } = &admission
    else {
        panic!("cold admission carries a deposit invoice")
    };
    let dead = dead.payment_hash.clone();
    let mut failed = f.buyer.channel("seller", admission).await.unwrap();
    assert!(matches!(&failed[..], [Message::DepositFailed { .. }]));
    let failed = failed.remove(0);
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 1);
    assert!(f.seller.channel("buyer", failed).await.unwrap().is_empty());
    assert_eq!(
        f.seller.gate(&gate("x", "buyer")).unwrap().status,
        Some(402),
        "nothing was paid, so nothing is admitted"
    );

    // Next request on the same connection quotes again and gets a new invoice.
    let again = f.buyer.peer_up("seller").await.unwrap();
    assert!(matches!(&again[..], [Message::Quote]));
    let admission = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0);
    let Message::Admission {
        invoice: Some(fresh),
        ..
    } = &admission
    else {
        panic!("fresh admission carries a deposit invoice")
    };
    assert_ne!(fresh.payment_hash, dead);
    let paid = f
        .buyer
        .channel("seller", admission)
        .await
        .unwrap()
        .remove(0);
    assert!(matches!(paid, Message::Paid { .. }));
    f.seller.channel("buyer", paid).await.unwrap();
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 2);
    assert_eq!(
        f.seller.gate(&gate("x", "buyer")).unwrap().decision,
        Decision::Allow
    );
    // The retry is one-shot: a healthy connection does not keep re-quoting.
    assert!(f.buyer.peer_up("seller").await.unwrap().is_empty());
}

/// Only a deposit the ledger closed as failed is retried. A forged report
/// from another peer, or for a ticket already paid, discards nothing.
#[tokio::test]
async fn deposit_failed_from_wrong_peer_or_for_paid_ticket_discards_nothing() {
    let f = Fixture::new();
    let id = f.prepare().await;
    assert!(
        f.seller
            .channel("attacker", Message::DepositFailed { id: id.clone() })
            .await
            .is_err()
    );
    f.seller
        .channel("buyer", Message::DepositFailed { id })
        .await
        .unwrap();
    assert_eq!(
        f.seller.gate(&gate("x", "buyer")).unwrap().decision,
        Decision::Allow
    );
}

/// Both sides publish `payment.lifecycle.v1` for one cold, deposited request,
/// each joined on its own host's OpenAI exchange ID, in the host's order.
#[tokio::test]
async fn both_sides_publish_lifecycle_events_joined_on_their_exchange() {
    let f = Fixture::new();
    let mut seller_events = f.seller.lifecycle.capture();
    let mut buyer_events = f.buyer.lifecycle.capture();
    let admission = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0);
    let paid = f
        .buyer
        .channel("seller", admission)
        .await
        .unwrap()
        .remove(0);
    for reply in f.seller.channel("buyer", paid).await.unwrap() {
        f.buyer.channel("seller", reply).await.unwrap();
    }
    assert!(
        buyer_events.try_recv().is_err(),
        "nothing before the exchange"
    );
    let decision = f
        .buyer
        .buyer_wait(
            "seller",
            "model",
            Some("buyer-exchange"),
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert_ne!(decision.decision, Decision::Deny);
    assert_eq!(
        f.seller
            .gate(&gate("seller-exchange", "buyer"))
            .unwrap()
            .decision,
        Decision::Allow
    );
    let (_, settlement) = f
        .seller
        .terminal(&terminal(
            "seller-exchange",
            json!({"prompt_tokens":2000,"completion_tokens":1000}),
        ))
        .await
        .unwrap()
        .unwrap();
    let paid = f
        .buyer
        .channel("seller", settlement)
        .await
        .unwrap()
        .remove(0);
    f.seller.channel("buyer", paid).await.unwrap();

    let drain = |events: &mut tokio::sync::mpsc::Receiver<crate::lifecycle::Event>| {
        std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>()
    };
    let buyer = drain(&mut buyer_events);
    let phases: Vec<_> = buyer.iter().map(|e| e.phase).collect();
    assert_eq!(
        phases,
        [
            "terms_accepted",
            "input_invoice_issued",
            "input_settlement_observed",
            "output_invoice_issued",
            "output_settlement_observed",
            "final_accounted"
        ]
    );
    assert!(
        buyer
            .iter()
            .all(|e| e.exchange_id == "buyer-exchange" && e.role == crate::lifecycle::Role::Payer)
    );
    assert_eq!(buyer[5].amount_msat, 3000);
    let seller = drain(&mut seller_events);
    let phases: Vec<_> = seller.iter().map(|e| e.phase).collect();
    assert_eq!(
        phases,
        [
            "input_invoice_issued",
            "input_settlement_observed",
            "delivered",
            "output_invoice_issued",
            "output_settlement_observed"
        ]
    );
    assert!(
        seller
            .iter()
            .all(|e| e.exchange_id == "seller-exchange"
                && e.role == crate::lifecycle::Role::Provider)
    );
    assert_eq!(seller[2].tokens, Some(1000));
}

/// Exhaust today's budget the way the live repro did: cap it at what is spent.
async fn exhaust_budget(f: &Fixture) {
    let available = f
        .buyer
        .service
        .routing_budget(RoutingBudgetRequest {
            request_intent: None,
        })
        .await
        .available_msat;
    f.buyer
        .service
        .ledger
        .set_policy(&Policy {
            mode: ApprovalMode::Automatic,
            daily_budget_msat: Some(100_000 - available),
            ..Default::default()
        })
        .unwrap();
    assert!(!f.buyer.buyer_enabled("seller").await);
}

async fn ready_ticket(f: &Fixture) {
    let id = f.prepare().await;
    f.buyer
        .channel("seller", Message::Ready { id })
        .await
        .unwrap();
}

#[tokio::test]
async fn exhausted_budget_refuses_a_route_with_a_prepared_ticket() {
    let f = Fixture::new();
    assert!(f.buyer.buyer_refusal("seller", "model").await.is_none());
    ready_ticket(&f).await;
    exhaust_budget(&f).await;
    let refused = f.buyer.buyer_refusal("seller", "model").await.unwrap();
    assert_eq!(refused.decision, Decision::Deny);
    assert_eq!(refused.status, Some(402));
    // A free model, or a peer holding no ticket, is left to the seller's gate.
    let free = f.buyer.buyer_refusal("seller", "free").await.unwrap();
    assert_ne!(free.decision, Decision::Deny);
    let other = f.buyer.buyer_refusal("other", "model").await.unwrap();
    assert_ne!(other.decision, Decision::Deny);
    // Restoring the budget dispatches again, through the kept ticket.
    f.buyer
        .service
        .ledger
        .set_policy(&Policy {
            mode: ApprovalMode::Automatic,
            daily_budget_msat: Some(100_000),
            ..Default::default()
        })
        .unwrap();
    assert!(f.buyer.buyer_refusal("seller", "model").await.is_none());
}

#[tokio::test]
async fn ready_ticket_is_not_claimed_once_policy_stops_buying() {
    let f = Fixture::new();
    ready_ticket(&f).await;
    f.buyer
        .service
        .ledger
        .set_policy(&Policy {
            mode: ApprovalMode::FreeOnly,
            daily_budget_msat: None,
            ..Default::default()
        })
        .unwrap();
    // Policy changed after the ticket became ready: it is neither claimed nor
    // dispatched, and the route is refused.
    assert!(
        f.buyer
            .buyer_wait(
                "seller",
                "model",
                None,
                std::time::Duration::from_millis(10)
            )
            .await
            .is_err()
    );
    assert_eq!(
        f.buyer
            .buyer_refusal("seller", "model")
            .await
            .unwrap()
            .status,
        Some(402)
    );
}

#[tokio::test]
async fn reconnect_is_not_held_by_a_wedged_seller_receipt_lookup() {
    let f = Fixture::new();
    f.buyer
        .service
        .ledger
        .set_pricing("model", Some(&price()))
        .unwrap();
    let admitted = f
        .buyer
        .service
        .serve_admit(ServeAdmitRequest {
            billing_revision: BillingRevision::V3,
            request_id: "unpaid".into(),
            buyer: "stranger".into(),
            model: "model".into(),
            pricing: price(),
        })
        .await
        .unwrap();
    let ServeAdmitResponse::Deposit { invoice, .. } = admitted else {
        panic!("not a deposit")
    };
    *f.network.slow_lookup.lock().unwrap() = Some(invoice.payment_hash);
    let opening =
        tokio::time::timeout(std::time::Duration::from_secs(2), f.buyer.peer_up("seller"))
            .await
            .expect("reconnect waited on a seller receipt lookup")
            .unwrap();
    assert!(matches!(opening[0], Message::Hello { .. }));
}

#[tokio::test]
async fn abandonment_cannot_replace_an_accepted_bill() {
    let f = Fixture::new();
    let id = f.prepare().await;
    f.seller.gate(&gate("x", "buyer")).unwrap();
    let (_, settlement) = f
        .seller
        .terminal(&terminal(
            "x",
            json!({"prompt_tokens":2000,"completion_tokens":1000}),
        ))
        .await
        .unwrap()
        .unwrap();
    f.buyer.channel("seller", settlement).await.unwrap();
    assert!(
        f.buyer
            .channel("seller", Message::Abandoned { id })
            .await
            .is_err()
    );
}

/// A buyer with no per-model ceiling, quoting a seller priced at `pricing`.
async fn quoted_unconfigured(pricing: Pricing) -> (Fixture, Message) {
    let mut f = Fixture::new();
    f.buyer.buying.clear();
    f.seller
        .service
        .ledger
        .set_pricing("model", Some(&pricing))
        .unwrap();
    f.buyer.peer_up("seller").await.unwrap();
    let admission = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0);
    (f, admission)
}

fn set_ceiling(f: &Fixture, input: Option<u64>, output: Option<u64>) {
    f.buyer
        .service
        .ledger
        .set_policy(&Policy {
            mode: ApprovalMode::Automatic,
            daily_budget_msat: Some(100_000),
            max_input_msat_per_million: input,
            max_output_msat_per_million: output,
        })
        .unwrap();
}

fn lab_price() -> Pricing {
    Pricing {
        input_msat_per_million: 50_000_000,
        output_msat_per_million: 50_000_000,
        cache_discount_percent: 0,
    }
}

fn assert_rate_refused(decision: &OpenAiExchangeDecision, header: &str) {
    assert_eq!(decision.decision, Decision::Deny);
    assert_eq!(decision.status, Some(403));
    let reason = decision.reason.as_deref().unwrap();
    assert!(reason.starts_with("Not paid: ") || reason.starts_with("Not sent: "));
    assert_eq!(reason.matches(". ").count(), 0, "one sentence: {reason}");
    let (name, value) = &decision.response_headers[0];
    assert_eq!(name, header);
    assert!(value.len() <= 1024);
    let terms: Value = serde_json::from_str(value).unwrap();
    assert_eq!(terms["refused"], "rate_above_ceiling");
    assert_eq!(terms["model"], "model");
    assert_eq!(terms["message"].as_str(), decision.reason.as_deref());
}

#[tokio::test]
async fn rates_at_the_default_ceiling_pay() {
    let (f, admission) = quoted_unconfigured(within_default_ceiling()).await;
    let paid = f.buyer.channel("seller", admission).await.unwrap();
    assert!(matches!(paid.first(), Some(Message::Paid { .. })));
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rates_over_the_ceiling_are_refused_at_once_before_any_deposit() {
    let (f, admission) = quoted_unconfigured(lab_price()).await;
    let Some((input, output)) = DEFAULT_RATE_CEILING else {
        // No shipped default: the lab price is paid, as before.
        assert!(f.buyer.channel("seller", admission).await.is_ok());
        return;
    };
    assert!(input < 50_000_000 && output < 50_000_000);
    assert!(f.buyer.channel("seller", admission).await.is_err());
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 0);
    // The waiting request is answered at once, not at its deadline.
    let started = std::time::Instant::now();
    let decision = f
        .buyer
        .buyer_wait("seller", "model", None, std::time::Duration::from_secs(20))
        .await
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_rate_refused(&decision, &f.buyer.header);
    let grouped = |n: u64| super::grouped(n);
    assert_eq!(
        decision.reason.as_deref().unwrap(),
        format!(
            "Not paid: seller seller's price for model (50,000,000 msat per million input tokens, 50,000,000 per million output) is above the default ceiling ({} and {}), which you have not set; raise it, or set a price for this model, to pay it.",
            grouped(input),
            grouped(output)
        )
    );
    // And so is the next request, before any quote.
    let next = f.buyer.rate_refusal("seller", "model").unwrap().unwrap();
    assert_rate_refused(&next, &f.buyer.header);
    // Another model or seller is unaffected.
    assert!(f.buyer.rate_refusal("seller", "other").unwrap().is_none());
    assert!(f.buyer.rate_refusal("other", "model").unwrap().is_none());
}

#[tokio::test]
async fn a_configured_ceiling_refuses_one_msat_over_it() {
    let pricing = Pricing {
        input_msat_per_million: 1_001,
        output_msat_per_million: 3_000,
        cache_discount_percent: 0,
    };
    let (f, admission) = quoted_unconfigured(pricing).await;
    set_ceiling(&f, Some(1_000), Some(3_000));
    assert!(f.buyer.channel("seller", admission).await.is_err());
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 0);
    let decision = f.buyer.rate_refusal("seller", "model").unwrap().unwrap();
    assert_rate_refused(&decision, &f.buyer.header);
    let terms: Value = serde_json::from_str(&decision.response_headers[0].1).unwrap();
    assert_eq!(terms["ceiling"]["input_msat_per_million"], 1_000);
    assert_eq!(terms["pricing"]["input_msat_per_million"], 1_001);
    assert_eq!(terms["ceiling"]["source"], "policy");
    assert!(
        decision
            .reason
            .unwrap()
            .contains("above your ceiling (1,000 and 3,000);")
    );
}

#[test]
fn rates_are_grouped_like_the_config_screen() {
    assert_eq!(grouped(0), "0");
    assert_eq!(grouped(999), "999");
    assert_eq!(grouped(1_000), "1,000");
    assert_eq!(grouped(1_500_000), "1,500,000");
    assert_eq!(grouped(u64::MAX), "no limit");
}

#[tokio::test]
async fn raising_the_ceiling_forgets_the_refusal_and_quotes_again() {
    let (f, admission) = quoted_unconfigured(lab_price()).await;
    set_ceiling(&f, Some(1_000), Some(1_000));
    assert!(f.buyer.channel("seller", admission).await.is_err());
    assert!(f.buyer.rate_refusal("seller", "model").unwrap().is_some());
    set_ceiling(&f, Some(50_000_000), Some(50_000_000));
    assert!(f.buyer.rate_refusal("seller", "model").unwrap().is_none());
    // The connected seller is quoted again, and its admission now pays.
    assert!(matches!(
        f.buyer.peer_up("seller").await.unwrap().as_slice(),
        [Message::Quote]
    ));
    let admission = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0);
    let paid = f.buyer.channel("seller", admission).await.unwrap();
    assert!(matches!(paid.first(), Some(Message::Paid { .. })));
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_fresh_connection_clears_the_sellers_refusals() {
    let (f, admission) = quoted_unconfigured(lab_price()).await;
    set_ceiling(&f, Some(1_000), Some(1_000));
    assert!(f.buyer.channel("seller", admission).await.is_err());
    assert!(f.buyer.rate_refusal("seller", "model").unwrap().is_some());
    // Cleared on disconnect, so the next request reaches `peer_up` and quotes.
    f.buyer.peer_down("seller");
    assert!(f.buyer.rate_refusal("seller", "model").unwrap().is_none());
}

#[tokio::test]
async fn a_per_model_price_replaces_the_policy_ceiling() {
    // The fixture's per-model ceiling (1,000,000 each way) is above a policy
    // ceiling of 10: the model's own entry decides, and the seller is paid.
    let f = Fixture::new();
    set_ceiling(&f, Some(10), Some(10));
    f.prepare().await;
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 1);
    // A per-model entry below the policy ceiling still refuses.
    let mut f = Fixture::new();
    set_ceiling(&f, Some(50_000_000), Some(50_000_000));
    f.buyer.buying.insert(
        "model".into(),
        Pricing {
            input_msat_per_million: 999_999,
            ..price()
        },
    );
    let admission = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0);
    assert!(f.buyer.channel("seller", admission).await.is_err());
    assert!(f.buyer.rate_refusal("seller", "model").unwrap().is_some());
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_ready_ticket_is_not_dispatched_above_a_lowered_ceiling() {
    let mut f = Fixture::new();
    ready_ticket(&f).await;
    // The ticket was readied under the per-model ceiling; the operator then
    // drops it, leaving a policy ceiling below the ticket's rates.
    f.buyer.buying.clear();
    set_ceiling(&f, Some(1_000), Some(1_000));
    let decision = f
        .buyer
        .buyer_wait(
            "seller",
            "model",
            None,
            std::time::Duration::from_millis(10),
        )
        .await
        .unwrap();
    assert_rate_refused(&decision, &f.buyer.header);
    // Its deposit was paid: the request is held unsent, not "not paid".
    assert!(decision.reason.unwrap().starts_with("Not sent: "));
    // The ticket is kept: a raised ceiling dispatches through it.
    set_ceiling(&f, Some(1_000_000), Some(1_000_000));
    let decision = f
        .buyer
        .buyer_wait(
            "seller",
            "model",
            None,
            std::time::Duration::from_millis(10),
        )
        .await
        .unwrap();
    assert_ne!(decision.decision, Decision::Deny);
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rates_over_the_ceiling_are_refused_at_once_even_with_a_short_discount() {
    let mut f = Fixture::new();
    // The model's entry wants a 50% cache discount; the seller offers none and
    // charges more than the entry allows: the rate refusal still answers at once.
    f.buyer.buying.insert(
        "model".into(),
        Pricing {
            input_msat_per_million: 10,
            ..price()
        },
    );
    f.seller
        .service
        .ledger
        .set_pricing(
            "model",
            Some(&Pricing {
                cache_discount_percent: 0,
                ..price()
            }),
        )
        .unwrap();
    let admission = f
        .seller
        .channel("buyer", Message::Quote)
        .await
        .unwrap()
        .remove(0);
    assert!(f.buyer.channel("seller", admission).await.is_err());
    assert!(f.buyer.rate_refusal("seller", "model").unwrap().is_some());
    assert_eq!(f.network.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn default_is_named_only_for_a_limit_the_payer_never_set() {
    let Some((_, default_output)) = DEFAULT_RATE_CEILING else {
        return;
    };
    let mut f = Fixture::new();
    f.buyer.buying.clear();
    // Input set by the payer, output left at the default; only output exceeded.
    set_ceiling(&f, Some(10), None);
    let over_output = Pricing {
        input_msat_per_million: 5,
        output_msat_per_million: default_output + 1,
        cache_discount_percent: 0,
    };
    let ceiling = f
        .buyer
        .exceeded_ceiling("model", &over_output)
        .unwrap()
        .unwrap();
    assert!(matches!(ceiling.source, CeilingSource::Default));
    // Exceeding the limit the payer set names it as theirs.
    let over_input = Pricing {
        input_msat_per_million: 11,
        ..over_output
    };
    let ceiling = f
        .buyer
        .exceeded_ceiling("model", &over_input)
        .unwrap()
        .unwrap();
    assert!(matches!(ceiling.source, CeilingSource::Policy));
}

#[test]
fn refusal_header_is_printable_ascii_and_keeps_the_sentence() {
    let f = Fixture::new();
    let ceiling = || Ceiling {
        input: 1,
        output: 1,
        source: CeilingSource::Policy,
    };
    // A non-ASCII model name is escaped, not dropped.
    let decision = f
        .buyer
        .rate_refused("seller", "模型", &price(), ceiling(), false);
    let value = &decision.response_headers[0].1;
    assert!(value.bytes().all(|b| (32..127).contains(&b)));
    let terms: Value = serde_json::from_str(value).unwrap();
    assert_eq!(terms["model"], "模型");
    assert_eq!(terms["message"].as_str(), decision.reason.as_deref());
    // A long model name drops the repeated terms before the sentence.
    let long = "m".repeat(500);
    let decision = f
        .buyer
        .rate_refused("seller", &long, &price(), ceiling(), false);
    let value = &decision.response_headers[0].1;
    assert!(value.len() <= 1024);
    let terms: Value = serde_json::from_str(value).unwrap();
    assert_eq!(terms["refused"], "rate_above_ceiling");
    assert!(terms["message"].as_str().unwrap().contains(&long));
    assert!(terms.get("pricing").is_none());
}
