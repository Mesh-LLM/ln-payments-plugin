//! Parity with the event contract mesh-llm's host emitted
//! (`network/payments/lifecycle.rs` before payments moved to this plugin) and
//! with the independent checker in Mesh-LLM/capsules-plugin.
use super::*;
use serde_json::json;

fn terms() -> AdmissionTerms {
    AdmissionTerms {
        id: "private-recovery".into(),
        model: "model".into(),
        pricing: mesh_llm_payments_types::pricing::Pricing {
            input_msat_per_million: 1000,
            output_msat_per_million: 2000,
            cache_discount_percent: 0,
        },
        deposit_msat: 3,
        min_payment_msat: mesh_llm_payments_types::metering::PAYMENT_FLOOR_MSAT,
    }
}

fn invoice(amount_msat: u64) -> Invoice {
    Invoice {
        bolt11: "lnbc-private".into(),
        payment_hash: "public-hash".into(),
        payee: "payee".into(),
        amount_msat: Some(amount_msat),
        expires_at_ms: 1000,
    }
}

fn payment(status: PaymentStatus) -> Transaction {
    Transaction {
        id: "wallet-private".into(),
        payment_hash: Some("public-hash".into()),
        inbound: false,
        amount_msat: 1000,
        fee_msat: 3,
        status,
        claiming: false,
        status_msg: None,
        created_at_ms: 0,
        settled_at_ms: Some(1),
    }
}

/// capsules-plugin's `event_ref_matches_an_independent_digest` vector: an
/// event body digested outside both codebases.
#[test]
fn digest_matches_the_independent_capsules_vector() {
    let body: Value = serde_json::from_str(r#"{"exchange_id":"exchange-one","event_ref":"f7c6847a6fbfcb6bc6ccc26bdb748b333e22946c70a4aa5684a60ba1c882b634","terms_digest":"7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d7d","phase":"terms_accepted","source":"payer_asserted","settlement":null,"segment":null,"payment_hash":null,"amount_msat":100}"#).unwrap();
    let mut blank = body.clone();
    blank["event_ref"] = json!("");
    assert_eq!(digest(&blank).unwrap(), body["event_ref"].as_str().unwrap());
}

#[test]
fn digest_refuses_what_the_host_refused() {
    assert!(digest(&json!({"amount_msat": MAX_SAFE_INTEGER})).is_some());
    assert!(digest(&json!({"amount_msat": MAX_SAFE_INTEGER + 1})).is_none());
    assert!(digest(&json!({"rate": 0.5})).is_none());
}

#[test]
fn terms_digest_excludes_the_private_recovery_id() {
    let mut request = terms();
    let exchange = Exchange::new(Role::Payer, "exchange-one", &request).unwrap();
    request.id = "different-secret".into();
    let again = Exchange::new(Role::Payer, "exchange-one", &request).unwrap();
    assert_eq!(exchange.terms_digest, again.terms_digest);
    request.deposit_msat = u64::MAX;
    assert!(Exchange::new(Role::Payer, "exchange-one", &request).is_none());
}

#[test]
fn event_contract_is_private_stable_and_self_checking() {
    let exchange = Exchange::new(Role::Payer, "exchange-one", &terms()).unwrap();
    let first = exchange.accepted(100).unwrap();
    assert_eq!(first.event_ref, exchange.accepted(100).unwrap().event_ref);
    assert_eq!(
        (first.phase, first.source, first.role),
        ("terms_accepted", "payer_asserted", Role::Payer)
    );
    let json = serde_json::to_value(&first).unwrap();
    let text = json.to_string();
    assert!(text.contains(r#""role":"payer""#));
    for secret in ["private-recovery", "preimage", "bolt11", "lnbc", "prompt"] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
    let mut blank = json.clone();
    blank["event_ref"] = json!("");
    assert_eq!(digest(&blank).unwrap(), first.event_ref);
}

#[test]
fn pending_and_failed_payments_never_emit_settlement() {
    let exchange = Exchange::new(Role::Payer, "exchange-two", &terms()).unwrap();
    assert!(
        exchange
            .settled(0, &payment(PaymentStatus::Pending))
            .is_none()
    );
    assert!(
        exchange
            .settled(0, &payment(PaymentStatus::Failed))
            .is_none()
    );
    let event = exchange
        .settled(0, &payment(PaymentStatus::Succeeded))
        .unwrap();
    assert_eq!(event.phase, "input_settlement_observed");
    assert_eq!(event.settlement, Some("terminal"));
    assert_eq!(event.payment_hash.as_deref(), Some("public-hash"));
    assert_eq!(
        (event.amount_msat, event.fee_msat, event.credited_msat),
        (1000, Some(3), None)
    );
}

#[test]
fn a_settlement_the_wallet_did_not_describe_has_exactly_the_original_members() {
    let exchange = Exchange::new(Role::Provider, "provider-plain", &terms()).unwrap();
    let event = exchange
        .received(1, &invoice(1000), &SettledReceived::default())
        .unwrap();
    let json = serde_json::to_value(&event).unwrap();
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let mut expected = vec![
        "exchange_id",
        "event_ref",
        "terms_digest",
        "role",
        "phase",
        "source",
        "settlement",
        "segment",
        "payment_hash",
        "amount_msat",
        "tokens",
    ];
    expected.sort_unstable();
    assert_eq!(keys, expected);
    let described = exchange
        .received(
            0,
            &invoice(1000),
            &SettledReceived {
                credited_msat: Some(995),
                fee_msat: Some(5),
            },
        )
        .unwrap();
    assert_eq!(
        described.amount_msat, 1000,
        "amount stays the invoice amount"
    );
    assert_eq!(
        (described.credited_msat, described.fee_msat),
        (Some(995), Some(5))
    );
}

#[test]
fn provider_asserts_delivered_watermark_and_issues_every_invoice() {
    let exchange = Exchange::new(Role::Provider, "provider-exchange", &terms()).unwrap();
    let delivered = exchange.delivered(42).unwrap();
    assert_eq!(
        (
            delivered.phase,
            delivered.tokens,
            delivered.amount_msat,
            delivered.settlement
        ),
        ("delivered", Some(42), 0, None)
    );
    let payer = Exchange::new(Role::Payer, "exchange", &terms()).unwrap();
    let issued = payer.invoice(1, &invoice(7)).unwrap();
    assert_eq!(
        (issued.phase, issued.source, issued.segment),
        ("output_invoice_issued", "provider_asserted", Some(1))
    );
}

#[tokio::test]
async fn nothing_is_emitted_before_dispatch_or_without_a_publisher() {
    let lifecycle = Lifecycle::default();
    lifecycle.payer_admitted(&terms(), 100, None);
    lifecycle.payer_dispatched("private-recovery", "exchange");
    let lifecycle = Lifecycle::default();
    let mut events = lifecycle.capture();
    lifecycle.payer_admitted(&terms(), 100, None);
    assert!(events.try_recv().is_err(), "no exchange yet");
    lifecycle.payer_dispatched("unknown", "exchange");
    assert!(events.try_recv().is_err());
    lifecycle.payer_dispatched("private-recovery", "exchange");
    assert_eq!(events.try_recv().unwrap().phase, "terms_accepted");
    lifecycle.abandoned("private-recovery");
    lifecycle.payer_settled("private-recovery", None, None, 1);
    assert!(events.try_recv().is_err(), "abandoned closes observation");
}
