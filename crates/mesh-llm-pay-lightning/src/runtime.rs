//! Standalone plugin. Money-moving behavior requires explicit isolated-state config.
use crate::{CHANNEL, Config, Engine, Message};
use anyhow::{Context, Result, ensure};
use mesh_llm_payments::service::PaymentService;
use mesh_llm_plugin::{
    PluginContext, PluginMetadata, PluginRuntime, SimplePlugin, mesh_channel, mesh_event_peer_down,
    mesh_event_peer_up, plugin_manifest, plugin_server_info,
};

use std::sync::Arc;

pub(crate) fn manifest(header: &str) -> mesh_llm_plugin::proto::PluginManifest {
    let mut hook = mesh_llm_plugin::openai_exchange::openai_exchange_hook("payment_gate");
    hook.phases = vec![
        "request_received".into(),
        "backend_selected".into(),
        "exchange_finished".into(),
    ];
    hook.required = true;
    hook.admission = true;
    hook.headers = vec![header.into()];
    hook.deadline_ms = 30_000;
    use mesh_llm_plugin::{
        config_boolean, config_path, config_schema, config_setting, config_string,
    };
    plugin_manifest()
        .item(
            config_schema("pay-lightning")
                .setting(config_setting("config_path", config_path()))
                .setting(config_setting("wallet_binding", config_string()).default_value(&"wallet"))
                .setting(
                    config_setting("allow_wallet_provisioning", config_boolean())
                        .default_value(&true),
                ),
        )
        .item(hook)
        .item(mesh_channel(CHANNEL))
        .operation_binding(mesh_llm_plugin::operation_binding(
            "wallet",
            "wallet.v1",
            "wallet_open",
            [
                "wallet_balance",
                "wallet_transactions",
                "wallet_create_invoice",
                "wallet_pay",
                "wallet_lookup",
                "wallet_wait_for_payment",
                "wallet_wait_for_arrival",
            ],
            true,
        ))
        .peer_blocks()
        .item(mesh_event_peer_up())
        .item(mesh_event_peer_down())
        // Published so the host's local API (`mesh-llm wallet …`) may dispatch it;
        // the host refuses operations a manifest does not list.
        .item(mesh_llm_plugin::operation::<serde_json::Value>(
            CONTROL_OPERATION,
            CONTROL_DESCRIPTION,
        ))
        .build()
}

const CONTROL_OPERATION: &str = "control";
const CONTROL_DESCRIPTION: &str =
    "Run one local operator command (balance, fund, send, policy, pricing, …).";

/// Turns the host's handshake settings into the wallet adapter and config.
type Setup = Box<
    dyn FnOnce(
            &str,
        ) -> Result<(
            Arc<dyn mesh_llm_wallet::provisioning::WalletFactory>,
            Config,
        )> + Send,
>;
type EngineSlot = Arc<std::sync::OnceLock<Arc<Engine>>>;

/// What `run_bound` needs from the operator's settings.
pub struct BoundSettings {
    pub wallet_binding: String,
    pub allow_wallet_provisioning: bool,
    pub config: Config,
}

/// Run the payment protocol with an in-process wallet adapter and a fixed
/// config; the host's settings are ignored.
pub async fn run(
    name: String,
    version: &str,
    factory: impl mesh_llm_wallet::provisioning::WalletFactory + 'static,
    config: Config,
) -> Result<()> {
    let factory: Arc<dyn mesh_llm_wallet::provisioning::WalletFactory> = Arc::new(factory);
    run_with_setup(
        name,
        version,
        Box::new(move |_| Ok((factory, config))),
        None,
    )
    .await
}

/// Run against the host-owned wallet binding. `settings` receives the
/// operator's `[plugin.settings]` JSON from the initialize handshake.
pub async fn run_bound(
    name: String,
    version: &str,
    settings: impl FnOnce(&str) -> Result<BoundSettings> + Send + 'static,
) -> Result<()> {
    let context = Arc::new(std::sync::OnceLock::new());
    let wallet_context = context.clone();
    let setup: Setup = Box::new(move |json| {
        let settings = settings(json)?;
        let factory: Arc<dyn mesh_llm_wallet::provisioning::WalletFactory> =
            Arc::new(crate::wallet_binding::InitializedWalletFactory {
                context: wallet_context,
                binding: settings.wallet_binding,
                allow_provisioning: settings.allow_wallet_provisioning,
            });
        Ok((factory, settings.config))
    });
    run_with_setup(name, version, setup, Some(context)).await
}

fn build_engine(
    namespace: &str,
    factory: Arc<dyn mesh_llm_wallet::provisioning::WalletFactory>,
    config: Config,
) -> Result<Arc<Engine>> {
    crate::diagnostics::configure(config.timing_log.clone(), config.error_log.clone())?;
    let directory = &config.data_dir;
    let service = Arc::new(PaymentService::with_factory(directory, factory)?);
    service.ledger.set_policy(&config.policy)?;
    for (model, price) in &config.prices {
        service.ledger.set_pricing(model, Some(price))?;
    }
    let mut engine = Engine::new(service, namespace, config.cap_msat, directory)?;
    engine.buying = config.buy_prices;
    engine.buy_peers = config.buy_peers;
    Ok(Arc::new(engine))
}

type Prepared = (
    Arc<dyn mesh_llm_wallet::provisioning::WalletFactory>,
    Config,
);

fn prepare(
    setup: &std::sync::Mutex<Option<Setup>>,
    settings_json: &str,
) -> mesh_llm_plugin::PluginResult<Prepared> {
    let result: Result<Prepared> = (|| {
        let setup = setup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .context("pay-lightning initialized twice")?;
        let (factory, config) = setup(settings_json)?;
        config.validate()?;
        Ok((factory, config))
    })();
    result.map_err(|error| {
        mesh_llm_plugin::PluginError::invalid_request(format!("pay-lightning settings: {error:#}"))
    })
}

async fn start(
    slot: &EngineSlot,
    namespace: &str,
    (factory, config): Prepared,
    context: &mut PluginContext<'static>,
    wallet_context: Option<Arc<std::sync::OnceLock<PluginContext<'static>>>>,
) -> Result<()> {
    let engine = build_engine(namespace, factory, config)?;
    ensure!(
        slot.set(engine.clone()).is_ok(),
        "pay-lightning initialized twice"
    );
    for model in engine.service.ledger.pricing()?.keys() {
        context
            .require_channels_for_model(model, &[(CHANNEL, &[])])
            .await?;
    }
    if let Some(wallet_context) = wallet_context {
        ensure!(
            context.host_supports(mesh_llm_plugin::operation_binding::CAPABILITY),
            "host does not support operation bindings"
        );
        let _ = wallet_context.set(context.owned());
    }
    engine.start().await?;
    observe_receipts(Arc::downgrade(&engine));
    Ok(())
}

fn engine_of(slot: &EngineSlot) -> Result<Arc<Engine>> {
    slot.get()
        .cloned()
        .context("pay-lightning is not initialized")
}

async fn run_with_setup(
    name: String,
    version: &str,
    setup: Setup,
    wallet_context: Option<Arc<std::sync::OnceLock<PluginContext<'static>>>>,
) -> Result<()> {
    let namespace: String = name.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let header = format!("x-plugin-{namespace}-pay-terms");
    let manifest = manifest(&header);
    let slot: EngineSlot = Arc::new(std::sync::OnceLock::new());
    let setup = Arc::new(std::sync::Mutex::new(Some(setup)));
    // Settings are parsed and validated in the handshake, so a bad config is
    // refused there; state opens afterwards, off the initialize deadline.
    let prepared = Arc::new(std::sync::Mutex::new(None));

    let metadata = payment_metadata(&name, version, manifest);
    let operations = control_operations(slot.clone());
    let messages = slot.clone();
    let events = slot.clone();
    let startup = slot.clone();
    let engine = slot.clone();
    let initialize = prepared.clone();
    let plugin = SimplePlugin::new(metadata)
        .with_operation_router(operations)
        .on_initialize(move |request, _context| {
            let setup = setup.clone();
            let prepared = initialize.clone();
            Box::pin(async move {
                let ready = prepare(&setup, &request.settings_json)?;
                *prepared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ready);
                Ok(())
            })
        })
        // After the handshake: opening a wallet must not consume the host's
        // initialize-response timeout. Manifest inspection never reaches here.
        .on_initialized(move |context| {
            let slot = startup.clone();
            let prepared = prepared.clone();
            let namespace = namespace.clone();
            let mut context = context.owned();
            let wallet_context = wallet_context.clone();
            Box::pin(async move {
                let ready = prepared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .context("pay-lightning settings missing")?;
                start(&slot, &namespace, ready, &mut context, wallet_context).await
            })
        })
        .on_channel_message(move |message, context| {
            let slot = messages.clone();
            let mut context = context.owned();
            Box::pin(async move {
                let engine = engine_of(&slot)?;
                let detached = serde_json::from_slice::<Message>(&message.body).is_ok_and(|body| {
                    matches!(
                        body,
                        Message::Settlement { .. }
                            | Message::Recover { .. }
                            | Message::Paid { .. }
                            | Message::Admission { .. }
                            | Message::Quote
                    )
                });
                let work = async move {
                    let result: Result<()> = async {
                        ensure!(message.channel == CHANNEL, "unexpected channel");
                        let message_body: Message = serde_json::from_slice(&message.body)?;
                        let _timing = crate::diagnostics::Timing::start(
                            "channel_receive_handler",
                            message_body.kind(),
                        );
                        let ready = match &message_body {
                            Message::Ready { id } => Some(id.clone()),
                            _ => None,
                        };
                        for reply in engine
                            .channel(&message.source_peer_id, message_body)
                            .await?
                        {
                            send(&mut context, &message.source_peer_id, &reply).await?;
                        }
                        if let Some(id) = ready
                            && engine.vouch_once(&message.source_peer_id, &id)?
                        {
                            context
                                .request_peer_block(mesh_llm_plugin::proto::PeerBlockRequest {
                                    change:
                                        mesh_llm_plugin::proto::peer_block_request::Change::Unblock
                                            as i32,
                                    peer_id: message.source_peer_id.clone(),
                                    ..Default::default()
                                })
                                .await?;
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(error) = &result {
                        record_error(error);
                    }
                    result
                };
                dispatch_channel_work(detached, work).await
            })
        })
        .on_mesh_event(move |event, context| {
            let slot = events.clone();
            let mut context = context.owned();
            Box::pin(async move {
                let engine = engine_of(&slot)?;
                if event.kind == mesh_llm_plugin::proto::mesh_event::Kind::PeerDown as i32
                    && let Some(peer) = &event.peer
                {
                    engine.peer_down(&peer.peer_id);
                }
                if event.kind == mesh_llm_plugin::proto::mesh_event::Kind::PeerUp as i32
                    && engine.cap_msat > 0
                    && let Some(peer) = event.peer
                    && engine.buy_peers.contains(&peer.peer_id)
                {
                    tokio::spawn(async move {
                        let result: Result<()> = async {
                            for message in engine.peer_up(&peer.peer_id).await? {
                                if let Err(error) =
                                    send(&mut context, &peer.peer_id, &message).await
                                {
                                    engine.peer_down(&peer.peer_id);
                                    return Err(error);
                                }
                            }
                            Ok(())
                        }
                        .await;
                        if let Err(error) = result {
                            record_error(&error);
                        }
                    });
                }

                Ok(())
            })
        })
        .with_openai_exchange_handler(move |_name, event, context| {
            let slot = engine.clone();
            let mut context = context.owned();
            Box::pin(async move {
                let engine = engine_of(&slot)
                    .map_err(|error| mesh_llm_plugin::PluginError::internal(error.to_string()))?;
                // Terminal wallet work is detached; it must not consume the hook's deadline.
                if event["phase"] == "exchange_finished" {
                    tokio::spawn(async move {
                        let result: Result<()> = async {
                            if let Some((peer, message)) = engine.terminal(&event).await? {
                                send(&mut context, &peer, &message).await?;
                            }
                            Ok(())
                        }
                        .await;
                        if let Err(error) = result {
                            record_error(&error);
                        }
                    });
                    return Ok(Default::default());
                }
                if event["phase"] == "backend_selected"
                    && let Some(peer) = event["target_peer_id"].as_str()
                    && engine.buyer_enabled(peer).await
                {
                    let result: Result<_> = async {
                        let model = event["model"].as_str().context("selected model missing")?;
                        tokio::time::timeout(std::time::Duration::from_secs(28), async {
                            for message in engine.peer_up(peer).await? {
                                if let Err(error) = send(&mut context, peer, &message).await {
                                    engine.peer_down(peer);
                                    return Err(error);
                                }
                            }
                            engine
                                .buyer_wait(peer, model, std::time::Duration::from_secs(28))
                                .await
                        })
                        .await
                        .context("seller admission deadline exceeded")?
                    }
                    .await;
                    return result.map_err(|error| {
                        mesh_llm_plugin::PluginError::internal(error.to_string())
                    });
                }
                let decision = engine
                    .gate_wait(&event, std::time::Duration::from_secs(28))
                    .await
                    .map_err(|error| mesh_llm_plugin::PluginError::internal(error.to_string()))?;
                if decision.decision
                    == mesh_llm_plugin::openai_exchange::OpenAiAdmissionDecision::Allow
                {
                    // Refill in parallel with inference, never on its hook deadline.
                    tokio::spawn(async move {
                        if let Err(error) = replenish(&engine, &event, &mut context).await {
                            record_error(&error);
                        }
                    });
                }
                Ok(decision)
            })
        });
    PluginRuntime::run(plugin).await
}

pub(crate) fn observe_receipts(engine: std::sync::Weak<Engine>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let Some(engine) = engine.upgrade() else {
                break;
            };
            // One sweep at a time, outside admission and buyer payment locks.
            // A wedged wallet lookup must not retain the engine indefinitely.
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                engine.refresh_receipts(),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => record_error(&error),
                Err(error) => record_error(&anyhow::Error::new(error)),
            }
        }
    });
}

async fn replenish(
    engine: &Engine,
    event: &serde_json::Value,
    context: &mut PluginContext<'_>,
) -> Result<()> {
    if let Some((peer, replies)) = engine.replenish(event).await? {
        for reply in replies {
            send(context, &peer, &reply).await?;
        }
    }
    Ok(())
}

async fn dispatch_channel_work(
    detached: bool,
    work: impl std::future::Future<Output = Result<()>> + Send + 'static,
) -> Result<()> {
    if detached {
        // Charges are durable; submissions retain the service payment lock.
        tokio::spawn(work);
        Ok(())
    } else {
        work.await
    }
}

async fn send(context: &mut PluginContext<'_>, peer: &str, message: &Message) -> Result<()> {
    let _timing = crate::diagnostics::Timing::start("channel_send", message.kind());
    context
        .send_json_channel(CHANNEL, peer, "payment", message)
        .await
}
fn record_error(error: &anyhow::Error) {
    crate::diagnostics::record_error(error);
}

/// The operator surface `mesh-llm wallet …` forwards to: one published
/// operation taking a `ControlCommand`. Peers never reach it; the host only
/// dispatches published operations from its local API.
fn control_operations(slot: EngineSlot) -> mesh_llm_plugin::OperationRouter {
    let mut operations = mesh_llm_plugin::OperationRouter::new();
    operations.add_raw(
        mesh_llm_plugin::operation_with_schema(
            CONTROL_OPERATION,
            CONTROL_DESCRIPTION,
            serde_json::Map::new(),
        ),
        move |request, _context| {
            let slot = slot.clone();
            Box::pin(async move {
                let engine = engine_of(&slot)?;
                let command: mesh_llm_payments::control::ControlCommand = request.arguments()?;
                // Durable settlement continues even if the caller goes away.
                let result = tokio::spawn(async move { engine.service.control(command).await })
                    .await
                    .map_err(|error| anyhow::anyhow!(error))
                    .and_then(|result| result);
                Ok(match result {
                    Ok(value) => rmcp::model::CallToolResult::structured(value),
                    Err(error) => rmcp::model::CallToolResult::structured_error(
                        serde_json::json!({ "message": error.to_string() }),
                    ),
                })
            })
        },
    );
    operations
}

fn payment_metadata(
    name: &str,
    version: &str,
    manifest: mesh_llm_plugin::proto::PluginManifest,
) -> PluginMetadata {
    PluginMetadata::new(
        name,
        version,
        plugin_server_info(
            name,
            version,
            "Lightning payments",
            "Plugin-owned credit, ledger and settlement",
            None::<String>,
        ),
    )
    .with_manifest(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn settlement_does_not_hold_ordered_handler() {
        let (release, blocked) = tokio::sync::oneshot::channel();
        let (finished, completion) = tokio::sync::oneshot::channel();
        dispatch_channel_work(true, async move {
            blocked.await?;
            let _ = finished.send(());
            Ok(())
        })
        .await
        .unwrap();
        dispatch_channel_work(false, async { Ok(()) })
            .await
            .unwrap();
        release.send(()).unwrap();
        completion.await.unwrap();
        assert!(
            dispatch_channel_work(false, async { anyhow::bail!("failure") })
                .await
                .is_err()
        );
    }

    #[test]
    fn manifest_requests_only_required_gate_metadata_and_channel() {
        let manifest = manifest("x-plugin-7061792d6c696768746e696e67-pay-terms");
        // Only the operator control surface is published; peers never reach it.
        assert_eq!(
            manifest
                .operations
                .iter()
                .map(|operation| operation.name.as_str())
                .collect::<Vec<_>>(),
            vec![CONTROL_OPERATION]
        );
        let hook = manifest.openai_exchange_hook.unwrap();
        assert!(hook.required && hook.admission && hook.metadata);
        assert!(!hook.request_body && !hook.response_body && !hook.delegate_signing_key);
        assert_eq!(
            hook.phases,
            vec!["request_received", "backend_selected", "exchange_finished"]
        );
        assert_eq!(manifest.mesh_channels.len(), 1);
        assert_eq!(manifest.mesh_channels[0].name, CHANNEL);
    }
}
