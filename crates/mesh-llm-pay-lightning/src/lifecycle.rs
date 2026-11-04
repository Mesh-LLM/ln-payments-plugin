//! Best-effort `payment.lifecycle.v1` observations, payer and provider side,
//! published on a local channel to the other plugins on this node. The same
//! event contract mesh-llm's host emitted before payments moved into this
//! plugin; never a ledger, replay service or payment authority.
use mesh_llm_payments_types::{
    contract::SettledReceived, metered_wire::AdmissionTerms, pricing::Pricing,
};
use mesh_llm_plugin::PluginContext;
use mesh_llm_wallet::{
    invoice::Invoice,
    provider::{PaymentStatus, Transaction},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};
use tokio::sync::mpsc;

pub const CHANNEL: &str = "payment.lifecycle.v1";
/// Exchanges awaiting or under observation; beyond this the oldest evidence
/// is dropped (incomplete evidence, never failed inference).
const OPEN_LIMIT: usize = 1024;
/// The largest integer the canonical digest accepts (JavaScript's safe range).
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Which side of a paid exchange observed an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Payer,
    Provider,
}

impl Role {
    fn asserted(self) -> &'static str {
        match self {
            Self::Payer => "payer_asserted",
            Self::Provider => "provider_asserted",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub exchange_id: String,
    pub event_ref: String,
    pub terms_digest: String,
    pub role: Role,
    pub phase: &'static str,
    pub source: &'static str,
    pub settlement: Option<&'static str>,
    pub segment: Option<u32>,
    pub payment_hash: Option<String>,
    pub amount_msat: u64,
    pub tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credited_msat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_msat: Option<u64>,
    /// The accepted terms' model and rates, on `terms_accepted` only: what
    /// the selected seller charges, as the terms the payer accepted state it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_msat_per_million: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_msat_per_million: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_discount_percent: Option<u8>,
}

/// One observed paid exchange, named by the host's OpenAI exchange ID so its
/// events join that exchange's `openai.exchange.v1` events.
#[derive(Clone, Debug)]
pub struct Exchange {
    exchange_id: String,
    terms_digest: String,
    role: Role,
    model: String,
    pricing: Pricing,
}

impl Exchange {
    pub fn new(role: Role, exchange_id: &str, terms: &AdmissionTerms) -> Option<Self> {
        Some(Self {
            exchange_id: exchange_id.into(),
            terms_digest: terms_digest(exchange_id, terms)?,
            role,
            model: terms.model.clone(),
            pricing: terms.pricing.clone(),
        })
    }

    /// Terms accepted by this side; `cap` is the payer's local spending cap.
    pub fn accepted(&self, cap: u64) -> Option<Event> {
        self.event(
            "terms_accepted",
            self.role.asserted(),
            None,
            None,
            cap,
            Extra {
                terms: true,
                ..Extra::default()
            },
        )
    }

    /// An invoice for `segment` (0 = deposit, 1 = final). The provider issues
    /// every invoice, so this is `provider_asserted` on both sides.
    pub fn invoice(&self, segment: u32, invoice: &Invoice) -> Option<Event> {
        self.event(
            if segment == 0 {
                "input_invoice_issued"
            } else {
                "output_invoice_issued"
            },
            "provider_asserted",
            Some(segment),
            Some(invoice.payment_hash.clone()),
            invoice.amount_msat.unwrap_or(0),
            Extra::default(),
        )
    }

    /// Payer side: the wallet's own record of an outgoing payment. Only a
    /// succeeded payment is a settlement.
    pub fn settled(&self, segment: u32, payment: &Transaction) -> Option<Event> {
        if payment.status != PaymentStatus::Succeeded {
            return None;
        }
        self.settlement(
            segment,
            payment.payment_hash.clone(),
            payment.amount_msat,
            Extra {
                fee_msat: Some(payment.fee_msat),
                ..Extra::default()
            },
        )
    }

    /// Provider side: the receiving wallet reported `invoice` settled.
    /// `amount_msat` stays the invoice amount; what the wallet credited and
    /// deducted ride alongside when it said.
    pub fn received(
        &self,
        segment: u32,
        invoice: &Invoice,
        settled: &SettledReceived,
    ) -> Option<Event> {
        self.settlement(
            segment,
            Some(invoice.payment_hash.clone()),
            invoice.amount_msat.unwrap_or(0),
            Extra {
                terms: false,
                tokens: None,
                credited_msat: settled.credited_msat,
                fee_msat: settled.fee_msat,
            },
        )
    }

    /// Provider side: the completion tokens the backend reported at settlement.
    pub fn delivered(&self, tokens: u64) -> Option<Event> {
        self.event(
            "delivered",
            "provider_asserted",
            None,
            None,
            0,
            Extra {
                tokens: Some(tokens),
                ..Extra::default()
            },
        )
    }

    pub fn final_amount(&self, amount: u64) -> Option<Event> {
        self.event(
            "final_accounted",
            "payer_asserted",
            None,
            None,
            amount,
            Extra::default(),
        )
    }

    fn settlement(
        &self,
        segment: u32,
        payment_hash: Option<String>,
        amount_msat: u64,
        extra: Extra,
    ) -> Option<Event> {
        self.event(
            if segment == 0 {
                "input_settlement_observed"
            } else {
                "output_settlement_observed"
            },
            "wallet_reported",
            Some(segment),
            payment_hash,
            amount_msat,
            extra,
        )
    }

    fn event(
        &self,
        phase: &'static str,
        source: &'static str,
        segment: Option<u32>,
        payment_hash: Option<String>,
        amount_msat: u64,
        extra: Extra,
    ) -> Option<Event> {
        let mut event = Event {
            exchange_id: self.exchange_id.clone(),
            event_ref: String::new(),
            terms_digest: self.terms_digest.clone(),
            role: self.role,
            phase,
            source,
            settlement: (source == "wallet_reported").then_some("terminal"),
            segment,
            payment_hash,
            amount_msat,
            tokens: extra.tokens,
            credited_msat: extra.credited_msat,
            fee_msat: extra.fee_msat,
            model: extra.terms.then(|| self.model.clone()),
            input_msat_per_million: extra.terms.then_some(self.pricing.input_msat_per_million),
            output_msat_per_million: extra.terms.then_some(self.pricing.output_msat_per_million),
            cache_discount_percent: (extra.terms && self.pricing.cache_discount_percent != 0)
                .then_some(self.pricing.cache_discount_percent),
        };
        event.event_ref = digest(&serde_json::to_value(&event).ok()?)?;
        Some(event)
    }
}

#[derive(Clone, Copy, Default)]
struct Extra {
    /// Carry the accepted terms' model and rates (`terms_accepted`).
    terms: bool,
    tokens: Option<u64>,
    credited_msat: Option<u64>,
    fee_msat: Option<u64>,
}

fn terms_digest(exchange_id: &str, terms: &AdmissionTerms) -> Option<String> {
    // Public terms only: never hash the bearer recovery ID.
    digest(&serde_json::json!({
        "exchange_id": exchange_id, "model": terms.model, "pricing": terms.pricing,
        "deposit_msat": terms.deposit_msat,
    }))
}

/// `HEX(SHA-256(JCS(value)))`, as the host computed `event_ref` and
/// `terms_digest`. Lifecycle values hold only strings, booleans, nulls and
/// integers, so RFC 8785 here is sorted keys and minimal form; a float or an
/// integer outside the safe range refuses the digest, as the host did.
pub fn digest(value: &Value) -> Option<String> {
    use sha2::{Digest, Sha256};
    let mut canonical = String::new();
    canonicalize(value, &mut canonical)?;
    Some(hex::encode(Sha256::digest(canonical.as_bytes())))
}

fn canonicalize(value: &Value, out: &mut String) -> Option<()> {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => out.push_str(&value.to_string()),
        Value::Number(number) => {
            let magnitude = number
                .as_u64()
                .or_else(|| number.as_i64().map(i64::unsigned_abs))?;
            if magnitude > MAX_SAFE_INTEGER {
                return None;
            }
            out.push_str(&number.to_string());
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonicalize(item, out)?;
            }
            out.push(']');
        }
        Value::Object(members) => {
            // RFC 8785 orders keys by UTF-16 code units; lifecycle keys are ASCII.
            let mut keys: Vec<&String> = members.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonicalize(&members[key], out)?;
            }
            out.push('}');
        }
    }
    Some(())
}

/// A buyer admission whose events wait for the exchange it is spent on.
#[derive(Clone)]
struct PayerAdmission {
    terms: AdmissionTerms,
    cap: u64,
    deposit: Option<(Invoice, Option<Transaction>)>,
}

/// A seller deposit received before the exchange that spends it.
#[derive(Clone)]
struct ProviderDeposit {
    invoice: Invoice,
    settled: SettledReceived,
}

/// The node's lifecycle publisher and the evidence held until an admission
/// is bound to an exchange. Nothing is queued before the plugin initializes.
#[derive(Default)]
pub struct Lifecycle {
    sender: OnceLock<mpsc::Sender<Event>>,
    payer: Mutex<HashMap<String, PayerAdmission>>,
    provider: Mutex<HashMap<String, ProviderDeposit>>,
    /// Admission ID to its observed exchange, until its final settlement.
    open: Mutex<HashMap<String, Exchange>>,
}

fn bounded_insert<V>(map: &Mutex<HashMap<String, V>>, key: &str, value: V) {
    let mut map = map.lock().unwrap();
    if map.len() >= OPEN_LIMIT && !map.contains_key(key) {
        map.clear();
    }
    map.insert(key.into(), value);
}

impl Lifecycle {
    /// Start publishing through the initialized plugin's context. One bounded
    /// queue; a full queue drops events rather than delay payment work.
    pub fn attach(&self, mut context: PluginContext<'static>) {
        let (sender, mut receiver) = mpsc::channel::<Event>(64);
        if self.sender.set(sender).is_err() {
            return;
        }
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                let Ok(body) = serde_json::to_vec(&event) else {
                    continue;
                };
                let message = mesh_llm_plugin::proto::ChannelMessage {
                    channel: CHANNEL.into(),
                    content_type: "application/json".into(),
                    body,
                    correlation_id: event.exchange_id.clone(),
                    message_kind: "lifecycle".into(),
                    ..Default::default()
                };
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    context.send_channel_message(message),
                )
                .await;
            }
        });
    }

    fn emit(&self, events: impl IntoIterator<Item = Option<Event>>) {
        let Some(sender) = self.sender.get() else {
            return;
        };
        for event in events.into_iter().flatten() {
            let _ = sender.try_send(event);
        }
    }

    /// Payer: an admission was accepted and its deposit (if any) submitted.
    pub fn payer_admitted(
        &self,
        terms: &AdmissionTerms,
        cap: u64,
        deposit: Option<(Invoice, Option<Transaction>)>,
    ) {
        bounded_insert(
            &self.payer,
            &terms.id,
            PayerAdmission {
                terms: terms.clone(),
                cap,
                deposit,
            },
        );
    }

    /// Payer: admission `id` is spent on `exchange_id`, the buyer's own
    /// OpenAI exchange. Emits what was observed before the exchange existed.
    pub fn payer_dispatched(&self, id: &str, exchange_id: &str) {
        let Some(admission) = self.payer.lock().unwrap().remove(id) else {
            return;
        };
        let Some(exchange) = Exchange::new(Role::Payer, exchange_id, &admission.terms) else {
            return;
        };
        let mut events = vec![exchange.accepted(admission.cap)];
        if let Some((invoice, payment)) = &admission.deposit {
            events.push(exchange.invoice(0, invoice));
            events.extend(payment.iter().map(|payment| exchange.settled(0, payment)));
        }
        self.emit(events);
        bounded_insert(&self.open, id, exchange);
    }

    /// Payer: the seller's settlement for `id` was checked and any final paid.
    pub fn payer_settled(
        &self,
        id: &str,
        invoice: Option<&Invoice>,
        payment: Option<&Transaction>,
        bill_msat: u64,
    ) {
        let Some(exchange) = self.open.lock().unwrap().remove(id) else {
            return;
        };
        self.emit([
            invoice.and_then(|invoice| exchange.invoice(1, invoice)),
            payment.and_then(|payment| exchange.settled(1, payment)),
            exchange.final_amount(bill_msat),
        ]);
    }

    /// Provider: the deposit for `id` settled in the receiving wallet.
    pub fn provider_deposit(&self, id: &str, invoice: &Invoice, settled: SettledReceived) {
        bounded_insert(
            &self.provider,
            id,
            ProviderDeposit {
                invoice: invoice.clone(),
                settled,
            },
        );
    }

    /// Provider: admission `id` is spent on the serving exchange `exchange_id`.
    pub fn provider_dispatched(&self, id: &str, exchange_id: &str, terms: &AdmissionTerms) {
        let Some(exchange) = Exchange::new(Role::Provider, exchange_id, terms) else {
            return;
        };
        if let Some(deposit) = self.provider.lock().unwrap().remove(id) {
            self.emit([
                exchange.invoice(0, &deposit.invoice),
                exchange.received(0, &deposit.invoice, &deposit.settled),
            ]);
        }
        bounded_insert(&self.open, id, exchange);
    }

    /// Provider: the backend's usage was billed; `invoice` is any final owed.
    /// Without a final invoice nothing more is observed for `id`.
    pub fn provider_billed(&self, id: &str, completion_tokens: u64, invoice: Option<&Invoice>) {
        let mut open = self.open.lock().unwrap();
        let Some(exchange) = open.get(id).cloned() else {
            return;
        };
        if invoice.is_none() {
            open.remove(id);
        }
        drop(open);
        self.emit([
            exchange.delivered(completion_tokens),
            invoice.and_then(|invoice| exchange.invoice(1, invoice)),
        ]);
    }

    /// Provider: the final invoice for `id` was received.
    pub fn provider_final_received(&self, id: &str, invoice: &Invoice) {
        let Some(exchange) = self.open.lock().unwrap().remove(id) else {
            return;
        };
        self.emit([exchange.received(1, invoice, &SettledReceived::default())]);
    }

    /// Either side: `id` closed without usage; nothing more is observed.
    pub fn abandoned(&self, id: &str) {
        self.open.lock().unwrap().remove(id);
        self.payer.lock().unwrap().remove(id);
        self.provider.lock().unwrap().remove(id);
    }

    #[cfg(test)]
    pub(crate) fn capture(&self) -> mpsc::Receiver<Event> {
        let (sender, receiver) = mpsc::channel(64);
        let _ = self.sender.set(sender);
        receiver
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
