use anoma::proto::services::{rpc_message, RpcResponse};
use anoma::types::MatchmakerMessage;
use libp2p::gossipsub::IdentTopic;
use libp2p::identity::Keypair;
use libp2p::identity::Keypair::Ed25519;
use libp2p::PeerId;
use prost::Message;
use thiserror::Error;
use tokio::sync::mpsc::Receiver;

use super::network_behaviour::Behaviour;

pub type Swarm = libp2p::Swarm<Behaviour>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed initializing the transport: {0}")]
    TransportError(std::io::Error),
    #[error("Failed to subscribe")]
    FailedSubscribtion(libp2p::gossipsub::error::SubscriptionError),
    #[error("Error with the network behavior: {0}")]
    Behavior(super::network_behaviour::Error),
}
type Result<T> = std::result::Result<T, Error>;

pub struct P2P {
    pub swarm: Swarm,
}

impl P2P {
    pub fn new(
        config: &anoma::config::IntentBroadcaster,
    ) -> Result<(Self, Option<Receiver<MatchmakerMessage>>)> {
        let local_key: Keypair = Ed25519(config.gossiper.key.clone());
        let local_peer_id: PeerId = PeerId::from(local_key.public());

        // Set up an encrypted TCP Transport over the Mplex and Yamux protocols
        let transport = libp2p::build_development_transport(local_key.clone())
            .map_err(Error::TransportError)?;

        let (gossipsub, matchmaker_event_receiver) =
            Behaviour::new(local_key, config).map_err(Error::Behavior)?;
        let swarm = Swarm::new(transport, gossipsub, local_peer_id);

        let mut p2p = Self { swarm };

        config
            .topics
            .iter()
            .try_for_each(|topic| {
                p2p.swarm
                    .intent_broadcaster_gossip
                    .subscribe(&IdentTopic::new(topic))
                    .map_err(Error::FailedSubscribtion)
                    // it returns bool signifying if it was already subscribed.
                    // discard because it can't be false as
                    // the config.topics is a hash set
                    .map(|_| ())
            })
            .expect("failed to subscribe to topic");

        Swarm::listen_on(&mut p2p.swarm, config.address.clone()).unwrap();

        for to_dial in &config.peers {
            match Swarm::dial_addr(&mut p2p.swarm, to_dial.clone()) {
                Ok(_) => tracing::info!("Dialed {:?}", to_dial.clone()),
                Err(e) => {
                    tracing::debug!(
                        "Dial {:?} failed: {:?}",
                        to_dial.clone(),
                        e
                    )
                }
            }
        }
        Ok((p2p, matchmaker_event_receiver))
    }

    pub async fn handle_mm_message(&mut self, mm_message: MatchmakerMessage) {
        self.swarm
            .intent_broadcaster_app
            .handle_mm_message(mm_message)
            .await
    }

    pub async fn handle_rpc_event(
        &mut self,
        event: rpc_message::Message,
    ) -> RpcResponse {
        match event {
            rpc_message::Message::Intent(
                anoma::proto::services::IntentMesage {
                    intent: None,
                    topic: _,
                },
            ) => {
                let result = format!(
                    "rpc intent command for topic {:?} is empty",
                    event
                );
                tracing::error!("{}", result);
                RpcResponse { result }
            }
            rpc_message::Message::Intent(
                anoma::proto::services::IntentMesage {
                    intent: Some(intent),
                    topic,
                },
            ) => {
                match self
                    .swarm
                    .intent_broadcaster_app
                    .apply_intent(intent.clone())
                {
                    Ok(true) => {
                        let mut intent_bytes = vec![];
                        intent.encode(&mut intent_bytes).unwrap();
                        match self
                            .swarm
                            .intent_broadcaster_gossip
                            .publish(IdentTopic::new(topic), intent_bytes)
                        {
                            Ok(message_id) => {
                                tracing::info!(
                                    "publish intent with message_id {}",
                                    message_id
                                );
                                RpcResponse {
                                    result: String::from(
                                        "Intent sent correctly",
                                    ),
                                }
                            }
                            Err(err) => {
                                tracing::error!(
                                    "error while publishing intent {:?}",
                                    err
                                );
                                RpcResponse {
                                    result: format!(
                                        "Failed to publish_intent {:?}",
                                        err
                                    ),
                                }
                            }
                        }
                    }
                    Ok(false) => RpcResponse {
                        result: String::from("Failed to apply the intent"),
                    },
                    Err(err) => {
                        tracing::error!(
                            "error while applying the intent {:?}",
                            err
                        );
                        RpcResponse {
                            result: format!(
                                "Failed to apply the intent {:?}",
                                err
                            ),
                        }
                    }
                }
            }
            rpc_message::Message::Dkg(dkg_msg) => {
                tracing::debug!(
                    "dkg not yet
        implemented {:?}",
                    dkg_msg
                );
                RpcResponse {
                    result: String::from(
                        "DKG
        application not yet implemented",
                    ),
                }
            }
            rpc_message::Message::Topic(
                anoma::proto::services::SubscribeTopicMessage {
                    topic: topic_str,
                },
            ) => {
                let topic = IdentTopic::new(&topic_str);
                match self.swarm.intent_broadcaster_gossip.subscribe(&topic) {
                    Ok(true) => {
                        let result = format!("Node subscribed to {}", topic);
                        tracing::info!("{}", result);
                        RpcResponse { result }
                    }
                    Ok(false) => {
                        let result =
                            format!("Node already subscribed to {}", topic);
                        tracing::info!("{}", result);
                        RpcResponse { result }
                    }
                    Err(err) => {
                        let result = format!(
                            "failed to subscribe to {}: {:?}",
                            topic, err
                        );
                        tracing::error!("{}", result);
                        RpcResponse { result }
                    }
                }
            }
        }
    }
}
