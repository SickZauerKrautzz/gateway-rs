use crate::*;
use link_packet::LinkPacket;
use semtech_udp::{
    server_runtime::{Error as SemtechError, Event, UdpRuntime},
    tx_ack,
};
use slog::{debug, info, o, warn, Logger};
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};

pub const DOWNLINK_TIMEOUT_SECS: u64 = 5;
pub const UPLINK_TIMEOUT_SECS: u64 = 6;

#[derive(Debug)]
pub struct Gateway {
    uplinks: Sender<LinkPacket>,
    downlinks: Receiver<LinkPacket>,
    udp_runtime: UdpRuntime,
}

impl Gateway {
    pub async fn new(
        uplinks: Sender<LinkPacket>,
        downlinks: Receiver<LinkPacket>,
        settings: &Settings,
    ) -> Result<Self> {
        let gateway = Gateway {
            uplinks,
            downlinks,
            udp_runtime: UdpRuntime::new(settings.listen_addr).await?,
        };
        Ok(gateway)
    }

    pub async fn run(&mut self, shutdown: triggered::Listener, logger: &Logger) -> Result {
        let logger = logger.new(o!("module" => "gateway"));
        info!(logger, "starting");
        loop {
            tokio::select! {
                _ = shutdown.clone() => {
                    info!(logger, "shutting down");
                    return Ok(())
                },
                event = self.udp_runtime.recv() =>
                    self.handle_udp_event(&logger, event).await?,
                downlink = self.downlinks.recv() => match downlink {
                    Some(packet) => self.handle_downlink(&logger, packet).await?,
                    None => {
                        warn!(logger, "ignoring closed downlinks channel");
                        continue;
                    }
                }
            }
        }
    }

    async fn handle_udp_event(&mut self, logger: &Logger, event: Event) -> Result {
        match event {
            Event::UnableToParseUdpFrame(buf) => {
                info!(logger, "ignoring semtech udp parsing error for {:?}", buf)
            }
            Event::NewClient((mac, addr)) => {
                info!(logger, "new packet forwarder client: {}, {}", mac, addr)
            }
            Event::UpdateClient((mac, addr)) => {
                info!(logger, "mac existed, but IP updated: {}, {}", mac, addr)
            }
            Event::PacketReceived(rxpk, gateway_mac) => {
                match LinkPacket::from_push_data(&rxpk, gateway_mac) {
                    Ok(packet) if packet.is_longfi() => {
                        info!(logger, "ignoring longfi packet");
                    }
                    Ok(packet) => {
                        let _ = self.uplinks.send(packet).await;
                    }
                    Err(err) => {
                        warn!(logger, "ignoring push_data: {:?}", err);
                    }
                }
            }
            Event::NoClientWithMac(_packet, mac) => {
                info!(
                    logger,
                    "ignoring send to client with unknown MAC: {:?}", mac
                )
            }
            Event::RawPacket(raw) => match raw {
                semtech_udp::Up::PushData(packet) => {
                    if let Some(rxpks) = packet.data.rxpk {
                        let mac = packet.gateway_mac;
                        for rxpk in rxpks {
                            info!(logger, "uplink {}, from {}", rxpk, mac)
                        }
                    }
                }
                _ => debug!(logger, "GWMP frame received {:?}", raw),
            },
        };
        Ok(())
    }

    async fn handle_downlink(&mut self, logger: &Logger, downlink: LinkPacket) -> Result {
        let (mut downlink_rx1, mut downlink_rx2) = (
            // first downlink
            self.udp_runtime
                .prepare_empty_downlink(downlink.gateway_mac),
            // 2nd downlink window if requested by the router response
            self.udp_runtime
                .prepare_empty_downlink(downlink.gateway_mac),
        );

        match downlink.to_pull_resp(false)? {
            None => Ok(()),
            Some(txpk) => {
                info!(
                    logger,
                    "rx1 downlink {} via {}",
                    txpk,
                    downlink_rx1.get_destination_mac()
                );
                downlink_rx1.set_packet(txpk);
                match downlink_rx1
                    .dispatch(Some(Duration::from_secs(DOWNLINK_TIMEOUT_SECS)))
                    .await
                {
                    // On a too early or too late error retry on the rx2 slot if available.
                    Err(SemtechError::Ack(tx_ack::Error::TooEarly))
                    | Err(SemtechError::Ack(tx_ack::Error::TooLate)) => {
                        if let Some(txpk) = downlink.to_pull_resp(true)? {
                            info!(
                                logger,
                                "rx2 downlink {} via {}",
                                txpk,
                                downlink_rx2.get_destination_mac()
                            );
                            downlink_rx2.set_packet(txpk);
                            match downlink_rx2
                                .dispatch(Some(Duration::from_secs(DOWNLINK_TIMEOUT_SECS)))
                                .await
                            {
                                Err(err) => {
                                    warn!(logger, "ignoring rx2 downlink error: {:?}", err);
                                    Ok(())
                                }
                                Ok(()) => Ok(()),
                            }
                        } else {
                            Ok(())
                        }
                    }
                    Err(err) => {
                        warn!(logger, "ignoring rx1 downlink error: {:?}", err);
                        Ok(())
                    }
                    Ok(()) => Ok(()),
                }
            }
        }
    }
}
