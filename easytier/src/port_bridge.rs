use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};

use anyhow::{bail, Context};
use tokio::io::copy_bidirectional;
use tokio::sync::Mutex;
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::common::config::PortBridgeRule;

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
    tasks: Arc<Mutex<HashMap<PortBridgeRule, BridgeTask>>>,
}

impl TcpPortBridge {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn apply_rules(&self, rules: &[PortBridgeRule]) -> anyhow::Result<()> {
        let mut guard = self.tasks.lock().await;
        let existing: HashSet<PortBridgeRule> = guard.keys().cloned().collect();
        let desired: HashSet<PortBridgeRule> = rules.iter().cloned().collect();

        let to_remove: Vec<PortBridgeRule> = existing.difference(&desired).cloned().collect();
        let to_add: Vec<PortBridgeRule> = desired.difference(&existing).cloned().collect();

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
            let bridge_task = Self::spawn_bridge_task(rule.clone())
                .await
                .with_context(|| format!("failed to start port bridge for {:?}", rule))?;
            {
                let mut guard = self.tasks.lock().await;
                guard.insert(rule, bridge_task);
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

    async fn spawn_bridge_task(rule: PortBridgeRule) -> anyhow::Result<BridgeTask> {
        match rule.proto.as_str() {
            "tcp" => Self::spawn_tcp_bridge_task(rule).await,
            "udp" => Self::spawn_udp_bridge_task(rule).await,
            other => bail!("unsupported port bridge protocol: {}", other),
        }
    }

    async fn spawn_tcp_bridge_task(rule: PortBridgeRule) -> anyhow::Result<BridgeTask> {
        let listen = rule.listen;
        let target = rule.target;

        let listener = TcpListener::bind(listen)
            .await
            .with_context(|| format!("failed to bind tcp bridge {}", listen))?;
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
                                Self::spawn_tcp_connection_handler(inbound, target);
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

    async fn spawn_udp_bridge_task(rule: PortBridgeRule) -> anyhow::Result<BridgeTask> {
        let listen = rule.listen;
        let target = rule.target;

        let socket = Arc::new(
            UdpSocket::bind(listen)
                .await
                .with_context(|| format!("failed to bind udp bridge {}", listen))?,
        );
        tracing::info!("udp bridge listening on {}, targeting {}", listen, target);

        let clients: Arc<Mutex<HashMap<SocketAddr, UdpClientEntry>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let cancel = CancellationToken::new();
        let cancel_token = cancel.clone();
        let socket_clone = socket.clone();
        let clients_clone = clients.clone();
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65_536];
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    recv_res = socket.recv_from(&mut buf) => {
                        match recv_res {
                            Ok((len, src_addr)) => {
                                if let Err(err) = Self::handle_udp_datagram(
                                    &socket_clone,
                                    &clients_clone,
                                    &cancel_token,
                                    listen,
                                    target,
                                    src_addr,
                                    &buf[..len],
                                ).await {
                                    tracing::error!("udp bridge handling error: {:?}", err);
                                }
                            }
                            Err(err) => {
                                tracing::error!("udp bridge recv error on {}: {:?}", listen, err);
                            }
                        }
                    }
                }
            }

            // cleanup client tasks
            let mut guard = clients_clone.lock().await;
            for (_, entry) in guard.drain() {
                entry.cancel.cancel();
                let _ = entry.handle.await;
            }
        });

        Ok(BridgeTask::new(cancel, handle))
    }

    async fn handle_udp_datagram(
        listener: &Arc<UdpSocket>,
        clients: &Arc<Mutex<HashMap<SocketAddr, UdpClientEntry>>>,
        cancel: &CancellationToken,
        listen_addr: SocketAddr,
        target_addr: SocketAddr,
        src_addr: SocketAddr,
        payload: &[u8],
    ) -> anyhow::Result<()> {
        let outbound_socket = {
            let mut guard = clients.lock().await;
            if let Some(entry) = guard.get(&src_addr) {
                entry.socket.clone()
            } else {
                let outbound = Arc::new(
                    UdpSocket::bind(unspecified_addr(listen_addr.ip()))
                        .await
                        .with_context(|| {
                            format!("failed to bind udp client socket for {}", src_addr)
                        })?,
                );
                outbound
                    .connect(target_addr)
                    .await
                    .with_context(|| format!("udp bridge failed to connect to {}", target_addr))?;

                let client_cancel = CancellationToken::new();
                let listener_clone = listener.clone();
                let cancel_clone = cancel.clone();
                let client_cancel_clone = client_cancel.clone();
                let outbound_clone = outbound.clone();
                let client_addr = src_addr;
                let handle = tokio::spawn(async move {
                    let mut buf = vec![0u8; 65_536];
                    loop {
                        tokio::select! {
                            _ = cancel_clone.cancelled() => break,
                            _ = client_cancel_clone.cancelled() => break,
                            recv_res = outbound_clone.recv(&mut buf) => {
                                match recv_res {
                                    Ok(len) => {
                                        if let Err(err) = listener_clone.send_to(&buf[..len], client_addr).await {
                                            tracing::debug!(
                                                "udp bridge failed to send back to {}: {:?}",
                                                client_addr,
                                                err
                                            );
                                            break;
                                        }
                                    }
                                    Err(err) => {
                                        tracing::error!("udp bridge outbound recv error: {:?}", err);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                });

                guard.insert(
                    src_addr,
                    UdpClientEntry {
                        socket: outbound.clone(),
                        cancel: client_cancel,
                        handle,
                    },
                );
                outbound
            }
        };

        if let Err(err) = outbound_socket.send(payload).await {
            tracing::debug!(
                "udp bridge failed to forward packet from {} to {}: {:?}",
                src_addr,
                target_addr,
                err
            );
        }
        Ok(())
    }

    fn spawn_tcp_connection_handler(mut inbound: TcpStream, target: SocketAddr) {
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

struct UdpClientEntry {
    socket: Arc<UdpSocket>,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

fn unspecified_addr(ip: IpAddr) -> SocketAddr {
    match ip {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    }
}
