use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
};

use anyhow::Context;
use tokio::io::copy_bidirectional;
use tokio::sync::Mutex;
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::common::config::TcpBridgeRule;

struct BridgeTask {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl BridgeTask {
    fn new(cancel: CancellationToken, handle: JoinHandle<()>) -> Self {
        Self { cancel, handle }
    }

    async fn stop(self) {
        self.cancel.cancel();
        if let Err(err) = self.handle.await {
            tracing::debug!("bridge task join finished with error: {:?}", err);
        }
    }
}

#[derive(Default)]
pub struct TcpPortBridge {
    tasks: Arc<Mutex<HashMap<TcpBridgeRule, BridgeTask>>>,
}

impl TcpPortBridge {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn apply_rules(&self, rules: &[TcpBridgeRule]) -> anyhow::Result<()> {
        let mut guard = self.tasks.lock().await;
        let existing: HashSet<TcpBridgeRule> = guard.keys().cloned().collect();
        let desired: HashSet<TcpBridgeRule> = rules.iter().cloned().collect();

        let to_remove: Vec<TcpBridgeRule> = existing.difference(&desired).cloned().collect();
        let to_add: Vec<TcpBridgeRule> = desired.difference(&existing).cloned().collect();

        let mut tasks_to_stop = Vec::new();
        for rule in to_remove {
            if let Some(task) = guard.remove(&rule) {
                tasks_to_stop.push(task);
            }
        }
        drop(guard);

        for task in tasks_to_stop {
            task.stop().await;
        }

        for rule in to_add {
            let forward_task = Self::spawn_bridge_task(rule.clone())
                .await
                .with_context(|| format!("failed to start port bridge for {:?}", rule))?;
            {
                let mut guard = self.tasks.lock().await;
                guard.insert(rule, forward_task);
            }
        }

        Ok(())
    }

    pub async fn shutdown(&self) {
        let mut guard = self.tasks.lock().await;
        let tasks: Vec<_> = guard.drain().map(|(_, task)| task).collect();
        drop(guard);

        for task in tasks {
            task.stop().await;
        }
    }

    async fn spawn_bridge_task(rule: TcpBridgeRule) -> anyhow::Result<BridgeTask> {
        let listen = rule.listen;
        let target = rule.target;

        let listener = TcpListener::bind(listen)
            .await
            .with_context(|| format!("failed to bind {}", listen))?;
        tracing::info!("tcp bridge listening on {}, targeting {}", listen, target);

        let cancel = CancellationToken::new();
        let cancel_token = cancel.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        tracing::info!("tcp bridge on {} shutting down", listen);
                        break;
                    }
                    accept_res = listener.accept() => {
                        match accept_res {
                            Ok((inbound, remote_addr)) => {
                                tracing::debug!(
                                    "tcp bridge accepted connection from {} on {}, forwarding to {}",
                                    remote_addr,
                                    listen,
                                    target
                                );
                                Self::spawn_connection_handler(inbound, target);
                            }
                            Err(err) => {
                                tracing::error!(
                                    "tcp bridge accept error on {}: {:?}",
                                    listen,
                                    err
                                );
                            }
                        }
                    }
                }
            }
        });

        Ok(BridgeTask::new(cancel, handle))
    }

    fn spawn_connection_handler(mut inbound: TcpStream, target: SocketAddr) {
        tokio::spawn(async move {
            match TcpStream::connect(target).await {
                Ok(mut outbound) => {
                    if let Err(err) = copy_bidirectional(&mut inbound, &mut outbound).await {
                        tracing::debug!(
                            "tcp bridge connection error ({:?} -> {}): {:?}",
                            inbound.peer_addr().ok(),
                            target,
                            err
                        );
                    }
                }
                Err(err) => {
                    tracing::error!("tcp bridge failed to connect to {}: {:?}", target, err);
                }
            }
        });
    }
}
