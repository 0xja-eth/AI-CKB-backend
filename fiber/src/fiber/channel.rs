#[cfg(debug_assertions)]
use crate::fiber::network::DebugEvent;
use crate::fiber::serde_utils::U64Hex;
use bitflags::bitflags;
use ckb_jsonrpc_types::BlockNumber;
use futures::future::OptionFuture;
use secp256k1::XOnlyPublicKey;
use tracing::{debug, error, info, trace, warn};

use crate::{
    ckb::{
        contracts::{get_cell_deps, get_script_by_contract, Contract},
        FundingRequest,
    },
    fiber::{
        config::{DEFAULT_MIN_SHUTDOWN_FEE, MAX_PAYMENT_TLC_EXPIRY_LIMIT, MIN_TLC_EXPIRY_DELTA},
        fee::{
            calculate_commitment_tx_fee, calculate_shutdown_tx_fee, calculate_tlc_forward_fee,
            shutdown_tx_size,
        },
        hash_algorithm::HashAlgorithm,
        key::blake2b_hash_with_salt,
        network::{
            get_chain_hash, sign_network_message, FiberMessageWithPeerId, SendOnionPacketCommand,
        },
        serde_utils::{CompactSignatureAsBytes, EntityHex, PubNonceAsBytes},
        types::{
            AcceptChannel, AddTlc, AnnouncementSignatures, BroadcastMessage, BroadcastMessageQuery,
            BroadcastMessageQueryFlags, ChannelAnnouncement, ChannelReady, ChannelUpdate,
            ClosingSigned, CommitmentSigned, EcdsaSignature, FiberChannelMessage, FiberMessage,
            Hash256, OpenChannel, PaymentOnionPacket, PeeledPaymentOnionPacket, Privkey, Pubkey,
            ReestablishChannel, RemoveTlc, RemoveTlcFulfill, RemoveTlcReason, RevokeAndAck,
            Shutdown, TlcErr, TlcErrPacket, TlcErrorCode, TxCollaborationMsg, TxComplete, TxUpdate,
            NO_SHARED_SECRET,
        },
        NetworkActorCommand, NetworkActorEvent, NetworkActorMessage, ASSUME_NETWORK_ACTOR_ALIVE,
    },
    invoice::{CkbInvoice, CkbInvoiceStatus, InvoiceStore},
    now_timestamp_as_millis_u64, NetworkServiceEvent,
};
use ckb_hash::{blake2b_256, new_blake2b};
use ckb_sdk::{Since, SinceType};
use ckb_types::{
    core::{
        Capacity, CapacityError, EpochNumberWithFraction, FeeRate, TransactionBuilder,
        TransactionView,
    },
    packed::{Bytes, CellInput, CellOutput, OutPoint, Script, Transaction},
    prelude::{AsTransactionBuilder, IntoTransactionView, Pack, Unpack},
};
use molecule::prelude::{Builder, Entity};
use musig2::{
    aggregate_partial_signatures,
    errors::{SigningError, VerifyError},
    secp::Point,
    sign_partial, verify_partial, AggNonce, CompactSignature, KeyAggContext, PartialSignature,
    PubNonce, SecNonce,
};
use ractor::{
    async_trait as rasync_trait, call, concurrency::Duration, Actor, ActorProcessingErr, ActorRef,
    OutputPort, RpcReplyPort, SpawnErr,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tentacle::secio::PeerId;
use thiserror::Error;
use tokio::sync::oneshot;

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
    u128,
};

// - `empty_witness_args`: 16 bytes, fixed to 0x10000000100000001000000010000000, for compatibility with the xudt
// - `pubkey`: 32 bytes, x only aggregated public key
// - `signature`: 64 bytes, aggregated signature
pub const FUNDING_CELL_WITNESS_LEN: usize = 16 + 32 + 64;

// - `empty_witness_args`: 16 bytes, fixed to 0x10000000100000001000000010000000, for compatibility with the xudt
// - `unlock_type`: 1 byte
// - `pubkey`: 32 bytes, x only aggregated public key
// - `signature`: 64 bytes, aggregated signature
pub const COMMITMENT_CELL_WITNESS_LEN: usize = 16 + 1 + 32 + 64;

// Some part of the code liberally gets previous commitment number, which is
// the current commitment number minus 1. We deliberately set initial commitment number to 1,
// so that we can get previous commitment point/number without checking if the channel
// is funded or not.
pub const INITIAL_COMMITMENT_NUMBER: u64 = 0;

// Whether we are receiving a channel update from node1 or node2.
// If the flag is set, it means the channel update is from node1, otherwise it is from node2.
pub const MESSAGE_OF_NODE1_FLAG: u32 = 0;

// Whether we are receiving a channel update from node1 or node2.
// If the flag is set, it means the channel update is from node2, otherwise it is from node1.
pub const MESSAGE_OF_NODE2_FLAG: u32 = 1;

// The channel is disabled, and no more tlcs can be added to the channel.
pub const CHANNEL_DISABLED_FLAG: u32 = 1;

const AUTO_SETDOWN_TLC_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum ChannelActorMessage {
    /// Command are the messages that are sent to the channel actor to perform some action.
    /// It is normally generated from a user request.
    Command(ChannelCommand),
    /// Some system events associated to a channel, such as the funding transaction confirmed.
    Event(ChannelEvent),
    /// PeerMessage are the messages sent from the peer.
    PeerMessage(FiberChannelMessage),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AddTlcResponse {
    pub tlc_id: u64,
}

#[derive(Clone)]
pub struct TlcNotification {
    pub channel_id: Hash256,
    pub tlc: AddTlcInfo,
    pub script: Script,
}

#[derive(Debug)]
pub enum ChannelCommand {
    TxCollaborationCommand(TxCollaborationCommand),
    // TODO: maybe we should automatically send commitment_signed message after receiving
    // tx_complete event.
    CommitmentSigned(),
    AddTlc(AddTlcCommand, RpcReplyPort<Result<AddTlcResponse, TlcErr>>),
    RemoveTlc(RemoveTlcCommand, RpcReplyPort<Result<(), String>>),
    Shutdown(ShutdownCommand, RpcReplyPort<Result<(), String>>),
    Update(UpdateCommand, RpcReplyPort<Result<(), String>>),
    #[cfg(test)]
    ReloadState(),
}

#[derive(Debug)]
pub enum TxCollaborationCommand {
    TxUpdate(TxUpdateCommand),
    TxComplete(),
}

#[derive(Debug, Clone)]
pub struct AddTlcCommand {
    pub amount: u128,
    pub payment_hash: Hash256,
    pub expiry: u64,
    pub hash_algorithm: HashAlgorithm,
    /// Onion packet for the next node
    pub onion_packet: Option<PaymentOnionPacket>,
    /// Shared secret used in forwarding.
    ///
    /// Save it for outbound (offered) TLC to backward errors.
    /// Use all zeros when no shared secrets are available.
    pub shared_secret: [u8; 32],
    pub previous_tlc: Option<(Hash256, u64)>,
}

#[derive(Debug)]
pub struct RemoveTlcCommand {
    pub id: u64,
    pub reason: RemoveTlcReason,
}

#[derive(Debug)]
pub struct ShutdownCommand {
    pub close_script: Script,
    pub fee_rate: FeeRate,
    pub force: bool,
}

#[derive(Debug)]
pub struct UpdateCommand {
    pub enabled: Option<bool>,
    pub tlc_expiry_delta: Option<u64>,
    pub tlc_minimum_value: Option<u128>,
    pub tlc_fee_proportional_millionths: Option<u128>,
}

#[derive(Debug)]
pub struct ChannelCommandWithId {
    pub channel_id: Hash256,
    pub command: ChannelCommand,
}

pub const DEFAULT_FEE_RATE: u64 = 1_000;
pub const DEFAULT_COMMITMENT_FEE_RATE: u64 = 1_000;
// The default commitment delay is 6 epochs = 24 hours.
pub const DEFAULT_COMMITMENT_DELAY_EPOCHS: u64 = 6;
// The min commitment delay is 1 epoch = 4 hours.
pub const MIN_COMMITMENT_DELAY_EPOCHS: u64 = 1;
// The max commitment delay is 84 epochs = 14 days.
pub const MAX_COMMITMENT_DELAY_EPOCHS: u64 = 84;
pub const DEFAULT_MAX_TLC_VALUE_IN_FLIGHT: u128 = u128::MAX;
pub const DEFAULT_MAX_TLC_NUMBER_IN_FLIGHT: u64 = 30;
pub const DEFAULT_MIN_TLC_VALUE: u128 = 0;
pub const SYS_MAX_TLC_NUMBER_IN_FLIGHT: u64 = 253;
pub const MAX_TLC_NUMBER_IN_FLIGHT: u64 = 125;

#[derive(Debug)]
pub struct TxUpdateCommand {
    pub transaction: Transaction,
}

pub struct OpenChannelParameter {
    pub funding_amount: u128,
    pub seed: [u8; 32],
    pub public_channel_info: Option<PublicChannelInfo>,
    pub funding_udt_type_script: Option<Script>,
    pub shutdown_script: Script,
    pub channel_id_sender: oneshot::Sender<Hash256>,
    pub commitment_fee_rate: Option<u64>,
    pub commitment_delay_epoch: Option<EpochNumberWithFraction>,
    pub funding_fee_rate: Option<u64>,
    pub max_tlc_value_in_flight: u128,
    pub max_tlc_number_in_flight: u64,
}

pub struct AcceptChannelParameter {
    pub funding_amount: u128,
    pub reserved_ckb_amount: u64,
    pub public_channel_info: Option<PublicChannelInfo>,
    pub seed: [u8; 32],
    pub open_channel: OpenChannel,
    pub shutdown_script: Script,
    pub channel_id_sender: Option<oneshot::Sender<Hash256>>,
    pub max_tlc_value_in_flight: u128,
    pub max_tlc_number_in_flight: u64,
}

pub enum ChannelInitializationParameter {
    /// To open a new channel to another peer, the funding amount,
    /// the temporary channel id a unique channel seed to generate
    /// channel secrets must be given.
    OpenChannel(OpenChannelParameter),
    /// To accept a new channel from another peer, the funding amount,
    /// a unique channel seed to generate unique channel id,
    /// original OpenChannel message and an oneshot
    /// channel to receive the new channel ID must be given.
    AcceptChannel(AcceptChannelParameter),
    /// Reestablish a channel with given channel id.
    ReestablishChannel(Hash256),
}

#[derive(Clone)]
pub struct ChannelSubscribers {
    pub pending_received_tlcs_subscribers: Arc<OutputPort<TlcNotification>>,
    pub settled_tlcs_subscribers: Arc<OutputPort<TlcNotification>>,
}

impl Default for ChannelSubscribers {
    fn default() -> Self {
        Self {
            pending_received_tlcs_subscribers: Arc::new(OutputPort::default()),
            settled_tlcs_subscribers: Arc::new(OutputPort::default()),
        }
    }
}

pub struct ChannelActor<S> {
    local_pubkey: Pubkey,
    remote_pubkey: Pubkey,
    network: ActorRef<NetworkActorMessage>,
    store: S,
    subscribers: ChannelSubscribers,
}

impl<S> ChannelActor<S>
where
    S: InvoiceStore + ChannelActorStateStore,
{
    pub fn new(
        local_pubkey: Pubkey,
        remote_pubkey: Pubkey,
        network: ActorRef<NetworkActorMessage>,
        store: S,
        subscribers: ChannelSubscribers,
    ) -> Self {
        Self {
            local_pubkey,
            remote_pubkey,
            network,
            store,
            subscribers,
        }
    }

    pub fn get_local_pubkey(&self) -> Pubkey {
        self.local_pubkey
    }

    pub fn get_remote_pubkey(&self) -> Pubkey {
        self.remote_pubkey
    }

    pub fn get_remote_peer_id(&self) -> PeerId {
        self.remote_pubkey.tentacle_peer_id()
    }

    pub async fn handle_peer_message(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        message: FiberChannelMessage,
    ) -> Result<(), ProcessingChannelError> {
        if state.reestablishing {
            match message {
                FiberChannelMessage::ReestablishChannel(ref reestablish_channel) => {
                    state.handle_reestablish_channel_message(reestablish_channel, &self.network)?;
                }
                _ => {
                    debug!("Ignoring message while reestablishing: {:?}", message);
                }
            }
            return Ok(());
        }

        match message {
            FiberChannelMessage::AnnouncementSignatures(announcement_signatures) => {
                if !state.is_public() {
                    return Err(ProcessingChannelError::InvalidState(
                        "Received AnnouncementSignatures message, but the channel is not public"
                            .to_string(),
                    ));
                }
                match state.state {
                    ChannelState::ChannelReady() => {}
                    ChannelState::AwaitingChannelReady(flags)
                        if flags.contains(AwaitingChannelReadyFlags::CHANNEL_READY) => {}
                    _ => {
                        return Err(ProcessingChannelError::InvalidState(format!(
                                "Received unexpected AnnouncementSignatures message in state {:?}, expecting state AwaitingChannelReady::CHANNEL_READY or ChannelReady",
                                state.state
                            )));
                    }
                }

                // TODO: check announcement_signatures validity here.
                let AnnouncementSignatures {
                    node_signature,
                    partial_signature,
                    ..
                } = announcement_signatures;
                state.update_remote_channel_announcement_signature(
                    node_signature,
                    partial_signature,
                );
                state.maybe_public_channel_is_ready(&self.network).await;
                Ok(())
            }
            FiberChannelMessage::AcceptChannel(accept_channel) => {
                state.handle_accept_channel_message(accept_channel)?;
                let old_id = state.get_id();
                state.fill_in_channel_id();
                self.network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ChannelAccepted(
                            state.get_remote_peer_id(),
                            state.get_id(),
                            old_id,
                            state.to_local_amount,
                            state.to_remote_amount,
                            state.get_funding_lock_script(),
                            state.funding_udt_type_script.clone(),
                            state.local_reserved_ckb_amount,
                            state.remote_reserved_ckb_amount,
                            state.funding_fee_rate,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                Ok(())
            }
            FiberChannelMessage::TxUpdate(tx) => {
                state.handle_tx_collaboration_msg(TxCollaborationMsg::TxUpdate(tx), &self.network)
            }
            FiberChannelMessage::TxComplete(tx) => {
                state.handle_tx_collaboration_msg(
                    TxCollaborationMsg::TxComplete(tx),
                    &self.network,
                )?;
                if let ChannelState::CollaboratingFundingTx(flags) = state.state {
                    if flags.contains(CollaboratingFundingTxFlags::COLLABRATION_COMPLETED) {
                        self.handle_commitment_signed_command(state)?;
                    }
                }
                Ok(())
            }
            FiberChannelMessage::CommitmentSigned(commitment_signed) => {
                self.handle_commitment_signed_peer_message(myself, state, commitment_signed)
                    .await
            }
            FiberChannelMessage::TxSignatures(tx_signatures) => {
                // We're the one who sent tx_signature first, and we received a tx_signature message.
                // This means that the tx_signature procedure is now completed. Just change state,
                // and exit.
                if state.should_local_send_tx_signatures_first() {
                    let new_witnesses: Vec<_> = tx_signatures
                        .witnesses
                        .into_iter()
                        .map(|x| x.pack())
                        .collect();
                    debug!(
                        "Updating funding tx witnesses of {:?} to {:?}",
                        state.must_get_funding_transaction().calc_tx_hash(),
                        new_witnesses.iter().map(|x| hex::encode(x.as_slice()))
                    );
                    state.funding_tx = Some(
                        state
                            .must_get_funding_transaction()
                            .as_advanced_builder()
                            .set_witnesses(new_witnesses)
                            .build()
                            .data(),
                    );
                    self.network
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::FundingTransactionPending(
                                state.must_get_funding_transaction().clone(),
                                state.must_get_funding_transaction_outpoint(),
                                state.get_id(),
                            ),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                    state.update_state(ChannelState::AwaitingChannelReady(
                        AwaitingChannelReadyFlags::empty(),
                    ));
                    return Ok(());
                };

                state.handle_tx_signatures(&self.network, Some(tx_signatures.witnesses))?;
                Ok(())
            }
            FiberChannelMessage::RevokeAndAck(revoke_and_ack) => {
                state.handle_revoke_and_ack_peer_message(&self.network, revoke_and_ack)?;
                Ok(())
            }
            FiberChannelMessage::ChannelReady(_channel_ready) => {
                let flags = match state.state {
                    ChannelState::AwaitingTxSignatures(flags) => {
                        if flags.contains(AwaitingTxSignaturesFlags::TX_SIGNATURES_SENT) {
                            AwaitingChannelReadyFlags::empty()
                        } else {
                            return Err(ProcessingChannelError::InvalidState(format!(
                                "received ChannelReady message, but we're not ready for ChannelReady, state is currently {:?}",
                                state.state
                            )));
                        }
                    }
                    ChannelState::AwaitingChannelReady(flags) => flags,
                    _ => {
                        return Err(ProcessingChannelError::InvalidState(format!(
                            "received ChannelReady message, but we're not ready for ChannelReady, state is currently {:?}", state.state
                        )));
                    }
                };
                let flags = flags | AwaitingChannelReadyFlags::THEIR_CHANNEL_READY;
                state.update_state(ChannelState::AwaitingChannelReady(flags));
                state.maybe_channel_is_ready(&self.network).await;
                Ok(())
            }
            FiberChannelMessage::AddTlc(add_tlc) => {
                self.handle_add_tlc_peer_message(state, add_tlc)
            }
            FiberChannelMessage::RemoveTlc(remove_tlc) => {
                self.handle_remove_tlc_peer_message(state, remove_tlc)
            }
            FiberChannelMessage::Shutdown(shutdown) => {
                let flags = match state.state {
                    ChannelState::ChannelReady() => ShuttingDownFlags::empty(),
                    ChannelState::ShuttingDown(flags)
                        if flags.contains(ShuttingDownFlags::THEIR_SHUTDOWN_SENT) =>
                    {
                        return Err(ProcessingChannelError::InvalidParameter(
                            "Received Shutdown message, but we're already in ShuttingDown state"
                                .to_string(),
                        ));
                    }
                    ChannelState::ShuttingDown(flags) => flags,
                    _ => {
                        return Err(ProcessingChannelError::InvalidState(format!(
                            "received Shutdown message, but we're not ready for Shutdown, state is currently {:?}",
                            state.state
                        )));
                    }
                };
                let shutdown_info = ShutdownInfo {
                    close_script: shutdown.close_script,
                    fee_rate: shutdown.fee_rate.as_u64(),
                    signature: None,
                };
                state.remote_shutdown_info = Some(shutdown_info);

                let mut flags = flags | ShuttingDownFlags::THEIR_SHUTDOWN_SENT;

                // Only automatically reply shutdown if only their shutdown message is sent.
                // If we are in a state other than only their shutdown is sent,
                // e.g. our shutdown message is also sent, or we are trying to force shutdown,
                // we should not reply.
                let should_we_reply_shutdown =
                    matches!(flags, ShuttingDownFlags::THEIR_SHUTDOWN_SENT);

                if state.check_valid_to_auto_accept_shutdown() && should_we_reply_shutdown {
                    let close_script = state.get_local_shutdown_script();
                    self.network
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                                state.get_remote_peer_id(),
                                FiberMessage::shutdown(Shutdown {
                                    channel_id: state.get_id(),
                                    close_script: close_script.clone(),
                                    fee_rate: FeeRate::from_u64(0),
                                }),
                            )),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    let shutdown_info = ShutdownInfo {
                        close_script,
                        fee_rate: 0,
                        signature: None,
                    };
                    state.local_shutdown_info = Some(shutdown_info);
                    flags |= ShuttingDownFlags::OUR_SHUTDOWN_SENT;
                    debug!("Auto accept shutdown ...");
                }
                state.update_state(ChannelState::ShuttingDown(flags));
                state.maybe_transition_to_shutdown(&self.network)?;
                Ok(())
            }
            FiberChannelMessage::ClosingSigned(closing) => {
                let ClosingSigned {
                    partial_signature,
                    channel_id,
                } = closing;

                if channel_id != state.get_id() {
                    return Err(ProcessingChannelError::InvalidParameter(
                        "Channel id mismatch".to_string(),
                    ));
                }

                // Note that we don't check the validity of the signature here.
                // we will check the validity when we're about to build the shutdown tx.
                // This may be or may not be a problem.
                // We do this to simplify the handling of the message.
                // We may change this in the future.
                // We also didn't check the state here.
                if let Some(shutdown_info) = state.remote_shutdown_info.as_mut() {
                    shutdown_info.signature = Some(partial_signature);
                }

                state.maybe_transition_to_shutdown(&self.network)?;
                Ok(())
            }
            FiberChannelMessage::ReestablishChannel(ref reestablish_channel) => {
                state.handle_reestablish_channel_message(reestablish_channel, &self.network)?;
                Ok(())
            }
            FiberChannelMessage::TxAbort(_)
            | FiberChannelMessage::TxInitRBF(_)
            | FiberChannelMessage::TxAckRBF(_) => {
                warn!("Received unsupported message: {:?}", &message);
                Ok(())
            }
        }
    }

    async fn get_tlc_error(
        &self,
        state: &mut ChannelActorState,
        error: &ProcessingChannelError,
    ) -> TlcErr {
        let error_code = match error {
            ProcessingChannelError::PeelingOnionPacketError(_) => TlcErrorCode::InvalidOnionPayload,
            ProcessingChannelError::TlcForwardFeeIsTooLow => TlcErrorCode::FeeInsufficient,
            ProcessingChannelError::TlcExpirySoon => TlcErrorCode::ExpiryTooSoon,
            ProcessingChannelError::TlcExpiryTooFar => TlcErrorCode::ExpiryTooFar,
            ProcessingChannelError::FinalInvoiceInvalid(status) => match status {
                CkbInvoiceStatus::Expired => TlcErrorCode::InvoiceExpired,
                CkbInvoiceStatus::Cancelled => TlcErrorCode::InvoiceCancelled,
                _ => TlcErrorCode::IncorrectOrUnknownPaymentDetails,
            },
            ProcessingChannelError::FinalIncorrectPreimage
            | ProcessingChannelError::FinalIncorrectPaymentHash => {
                TlcErrorCode::IncorrectOrUnknownPaymentDetails
            }
            ProcessingChannelError::FinalIncorrectHTLCAmount => {
                TlcErrorCode::FinalIncorrectTlcAmount
            }
            ProcessingChannelError::IncorrectTlcExpiry => TlcErrorCode::IncorrectTlcExpiry,
            ProcessingChannelError::IncorrectFinalTlcExpiry => {
                TlcErrorCode::FinalIncorrectExpiryDelta
            }
            ProcessingChannelError::TlcAmountIsTooLow => TlcErrorCode::AmountBelowMinimum,
            ProcessingChannelError::TlcNumberExceedLimit
            | ProcessingChannelError::TlcAmountExceedLimit
            | ProcessingChannelError::TlcValueInflightExceedLimit
            | ProcessingChannelError::WaitingTlcAck => TlcErrorCode::TemporaryChannelFailure,
            ProcessingChannelError::InvalidState(error) => match state.state {
                // we can not revert back up `ChannelReady` after `ShuttingDown`
                ChannelState::Closed(_) | ChannelState::ShuttingDown(_) => {
                    TlcErrorCode::PermanentChannelFailure
                }
                ChannelState::ChannelReady() => {
                    if error.contains("channel is not public or disabled") {
                        TlcErrorCode::TemporaryChannelFailure
                    } else {
                        // we expect `ChannelReady` will be both OK for tlc forwarding,
                        // so here are the unreachable point in normal workflow,
                        // set `TemporaryNodeFailure` for general temporary failure of the processing node here
                        assert!(false, "unreachable point in normal workflow");
                        TlcErrorCode::TemporaryNodeFailure
                    }
                }
                // otherwise, channel maybe not ready
                _ => TlcErrorCode::TemporaryChannelFailure,
            },
            ProcessingChannelError::RepeatedProcessing(_) => TlcErrorCode::TemporaryChannelFailure,
            ProcessingChannelError::SpawnErr(_)
            | ProcessingChannelError::Musig2SigningError(_)
            | ProcessingChannelError::Musig2VerifyError(_)
            | ProcessingChannelError::CapacityError(_) => TlcErrorCode::TemporaryNodeFailure,
            ProcessingChannelError::InvalidParameter(_) => {
                TlcErrorCode::IncorrectOrUnknownPaymentDetails
            }
            ProcessingChannelError::TlcForwardingError(_) => {
                unreachable!("TlcForwardingError should be handled before this point")
            }
        };

        let channel_update = if error_code.is_update() {
            state.try_create_channel_update_message(&self.network).await
        } else {
            None
        };
        TlcErr::new_channel_fail(
            error_code,
            state.local_pubkey,
            state.must_get_funding_transaction_outpoint(),
            channel_update,
        )
    }

    async fn handle_commitment_signed_peer_message(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        commitment_signed: CommitmentSigned,
    ) -> Result<(), ProcessingChannelError> {
        // build commitment tx and verify signature from remote, if passed send ACK for partner
        state.verify_commitment_signed_and_send_ack(commitment_signed, &self.network)?;
        self.flush_staging_tlc_operations(myself, state).await;
        Ok(())
    }

    async fn flush_staging_tlc_operations(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
    ) {
        let pending_apply_tlcs = state.tlc_state.commit_remote_tlcs();
        for tlc_info in pending_apply_tlcs {
            match tlc_info {
                TlcKind::AddTlc(add_tlc) => {
                    assert!(add_tlc.is_received());
                    if let Err(e) = self.apply_add_tlc_operation(myself, state, &add_tlc).await {
                        let tlc_err = match e.source {
                            // If we already have TlcErr, we can directly use it to send back to the peer.
                            ProcessingChannelError::TlcForwardingError(tlc_err) => tlc_err,
                            _ => {
                                let error_detail = self.get_tlc_error(state, &e.source).await;
                                #[cfg(debug_assertions)]
                                self.network
                                    .clone()
                                    .send_message(NetworkActorMessage::new_notification(
                                        NetworkServiceEvent::DebugEvent(DebugEvent::AddTlcFailed(
                                            state.get_local_peer_id(),
                                            add_tlc.payment_hash,
                                            error_detail.clone(),
                                        )),
                                    ))
                                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                                error_detail
                            }
                        };
                        let error_packet = TlcErrPacket::new(
                            tlc_err,
                            // There's no shared secret stored in the received TLC, use the one found in the peeled onion packet.
                            &e.shared_secret,
                        );
                        self.register_retryable_tlc_remove(
                            myself,
                            state,
                            add_tlc.tlc_id,
                            RemoveTlcReason::RemoveTlcFail(error_packet),
                        )
                        .await;
                    }
                }
                TlcKind::RemoveTlc(remove_tlc) => {
                    let _ = self
                        .apply_remove_tlc_operation(myself, state, remove_tlc)
                        .await
                        .map_err(|e| {
                            error!("Error handling apply_remove_tlc_operation: {:?}", e);
                        });
                }
            }
        }
    }

    async fn try_to_relay_remove_tlc(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        tlc_info: &AddTlcInfo,
        remove_reason: RemoveTlcReason,
    ) {
        assert!(tlc_info.is_offered());
        let (previous_channel_id, previous_tlc) =
            tlc_info.previous_tlc.expect("expect previous tlc");
        assert!(tlc_info.is_offered());
        assert!(previous_tlc.is_received());
        assert!(previous_channel_id != state.get_id());

        let remove_reason = remove_reason.clone().backward(&tlc_info.shared_secret);
        self.register_retryable_relay_tlc_remove(
            myself,
            state,
            previous_tlc.into(),
            previous_channel_id,
            remove_reason,
        )
        .await;
    }

    async fn try_to_settle_down_tlc(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        tlc_id: u64,
    ) {
        let tlc_info = state.get_received_tlc(tlc_id).expect("expect tlc");
        let preimage = tlc_info
            .payment_preimage
            .or_else(|| self.store.get_invoice_preimage(&tlc_info.payment_hash));

        let preimage = if let Some(preimage) = preimage {
            preimage
        } else {
            return;
        };

        let mut remove_reason = RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage,
        });
        let tlc = tlc_info.clone();
        if let Some(invoice) = self.store.get_invoice(&tlc.payment_hash) {
            let status = self.get_invoice_status(&invoice);
            match status {
                CkbInvoiceStatus::Expired => {
                    remove_reason = RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                        TlcErr::new(TlcErrorCode::InvoiceExpired),
                        &tlc.shared_secret,
                    ));
                }
                CkbInvoiceStatus::Cancelled => {
                    remove_reason = RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                        TlcErr::new(TlcErrorCode::InvoiceCancelled),
                        &tlc.shared_secret,
                    ));
                }
                CkbInvoiceStatus::Paid => {
                    // we have already checked invoice status in apply_add_tlc_operation_with_peeled_onion_packet
                    // this maybe happened when process is killed and restart
                    error!("invoice already paid, ignore");
                }
                _ => {
                    self.store
                        .update_invoice_status(&tlc.payment_hash, CkbInvoiceStatus::Paid)
                        .expect("update invoice status error");
                }
            }
        }

        self.register_retryable_tlc_remove(myself, state, tlc.tlc_id, remove_reason)
            .await;
    }

    async fn apply_add_tlc_operation(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        add_tlc: &AddTlcInfo,
    ) -> Result<(), ProcessingChannelErrorWithSharedSecret> {
        // If needed, shared secret also get be extracted from the encrypted onion packet:
        // - Extract public key from onion_packet[1..34]
        // - Obtain share secret using DH Key Exchange from the public key and the network private key stored in the network actor state.
        if let Some(peeled_onion_packet) = self
            .try_add_tlc_peel_onion_packet(state, add_tlc)
            .await
            .map_err(ProcessingChannelError::without_shared_secret)?
        {
            let shared_secret = peeled_onion_packet.shared_secret.clone();
            self.apply_add_tlc_operation_with_peeled_onion_packet(
                state,
                add_tlc,
                peeled_onion_packet,
            )
            .await
            .map_err(move |err| err.with_shared_secret(shared_secret))?;
        }

        if let Some(ref udt_type_script) = state.funding_udt_type_script {
            self.subscribers
                .pending_received_tlcs_subscribers
                .send(TlcNotification {
                    tlc: add_tlc.clone(),
                    channel_id: state.get_id(),
                    script: udt_type_script.clone(),
                });
        }

        // we don't need to settle down the tlc if it is not the last hop here,
        // some e2e tests are calling AddTlc manually, so we can not use onion packet to
        // check whether it's the last hop here, maybe need to revisit in future.
        self.try_to_settle_down_tlc(myself, state, add_tlc.tlc_id.into())
            .await;

        warn!("finished check tlc for peer message: {:?}", &add_tlc.tlc_id);
        Ok(())
    }

    async fn try_add_tlc_peel_onion_packet(
        &self,
        state: &mut ChannelActorState,
        add_tlc: &AddTlcInfo,
    ) -> Result<Option<PeeledPaymentOnionPacket>, ProcessingChannelError> {
        state.check_tlc_expiry(add_tlc.expiry)?;

        assert!(state.get_received_tlc(add_tlc.tlc_id.into()).is_some());

        Ok(
            OptionFuture::from(add_tlc.onion_packet.clone().map(|onion_packet| {
                self.peel_onion_packet(onion_packet, add_tlc.payment_hash.clone())
            }))
            .await
            .transpose()?,
        )
    }

    async fn apply_add_tlc_operation_with_peeled_onion_packet(
        &self,
        state: &mut ChannelActorState,
        add_tlc: &AddTlcInfo,
        peeled_onion_packet: PeeledPaymentOnionPacket,
    ) -> Result<(), ProcessingChannelError> {
        let payment_hash = add_tlc.payment_hash;
        let received_amount = add_tlc.amount;
        let forward_amount = peeled_onion_packet.current.amount;

        if peeled_onion_packet.is_last() {
            if forward_amount != add_tlc.amount {
                return Err(ProcessingChannelError::FinalIncorrectHTLCAmount);
            }

            if add_tlc.expiry < peeled_onion_packet.current.expiry {
                return Err(ProcessingChannelError::IncorrectFinalTlcExpiry);
            }
            if add_tlc.expiry < now_timestamp_as_millis_u64() + MIN_TLC_EXPIRY_DELTA {
                return Err(ProcessingChannelError::TlcExpirySoon);
            }

            if let Some(invoice) = self.store.get_invoice(&payment_hash) {
                let invoice_status = self.get_invoice_status(&invoice);
                if invoice_status != CkbInvoiceStatus::Open {
                    return Err(ProcessingChannelError::FinalInvoiceInvalid(invoice_status));
                }
                self.store
                    .update_invoice_status(&payment_hash, CkbInvoiceStatus::Received)
                    .expect("update invoice status failed");
            }

            // if this is the last hop, store the preimage.
            // though we will RemoveTlcFulfill the TLC in try_to_settle_down_tlc function,
            // here we can do error check early here for better error handling.
            let preimage = peeled_onion_packet
                .current
                .payment_preimage
                .or_else(|| self.store.get_invoice_preimage(&add_tlc.payment_hash));

            if let Some(preimage) = preimage {
                let filled_payment_hash: Hash256 = add_tlc.hash_algorithm.hash(preimage).into();
                if add_tlc.payment_hash != filled_payment_hash {
                    return Err(ProcessingChannelError::FinalIncorrectPreimage);
                }
                state.set_received_tlc_preimage(add_tlc.tlc_id.into(), Some(preimage));
            } else {
                return Err(ProcessingChannelError::FinalIncorrectPaymentHash);
            }
        } else {
            match state.public_channel_info.as_ref() {
                Some(public_channel_info) if public_channel_info.enabled => {
                    let min_tlc_value = public_channel_info.tlc_min_value;
                    if min_tlc_value > received_amount {
                        return Err(ProcessingChannelError::TlcAmountIsTooLow);
                    }

                    if add_tlc.expiry
                        < peeled_onion_packet.current.expiry + public_channel_info.tlc_expiry_delta
                    {
                        return Err(ProcessingChannelError::IncorrectTlcExpiry);
                    }

                    assert!(received_amount >= forward_amount);
                    let forward_fee = received_amount.saturating_sub(forward_amount);
                    let fee_rate: u128 = public_channel_info.tlc_fee_proportional_millionths;

                    let expected_fee = calculate_tlc_forward_fee(forward_amount, fee_rate);
                    if expected_fee.is_err() || forward_fee < expected_fee.clone().unwrap() {
                        error!(
                            "too low forward_fee: {}, expected_fee: {:?}",
                            forward_fee, expected_fee
                        );
                        return Err(ProcessingChannelError::TlcForwardFeeIsTooLow);
                    }
                    // if this is not the last hop, forward TLC to next hop
                    self.handle_forward_onion_packet(
                        state,
                        add_tlc.payment_hash,
                        peeled_onion_packet.clone(),
                        add_tlc.tlc_id.into(),
                    )
                    .await?;
                }
                _ => {
                    // if we don't have public channel info, we can not forward the TLC
                    // this may happended some malicious sender build a invalid onion router
                    return Err(ProcessingChannelError::InvalidState(
                        "Received AddTlc message, but the channel is not public or disabled"
                            .to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn handle_add_tlc_peer_message(
        &self,
        state: &mut ChannelActorState,
        add_tlc: AddTlc,
    ) -> Result<(), ProcessingChannelError> {
        // TODO: here we only check the error which sender didn't follow agreed rules,
        //       if any error happened here we need go to shutdown procedure

        state.check_for_tlc_update(Some(add_tlc.amount), false, false)?;
        let tlc_info = state.create_inbounding_tlc(add_tlc.clone())?;
        state.check_insert_tlc(&tlc_info)?;
        state
            .tlc_state
            .add_remote_tlc(TlcKind::AddTlc(tlc_info.clone()));
        state.increment_next_received_tlc_id();
        Ok(())
    }

    fn handle_remove_tlc_peer_message(
        &self,
        state: &mut ChannelActorState,
        remove_tlc: RemoveTlc,
    ) -> Result<(), ProcessingChannelError> {
        state.check_for_tlc_update(None, false, false)?;
        // TODO: here if we received a invalid remove tlc, it's maybe a malioucious peer,
        // maybe we need to go through shutdown process for this error
        state
            .check_remove_tlc_with_reason(TLCId::Offered(remove_tlc.tlc_id), &remove_tlc.reason)?;
        let tlc_kind = TlcKind::RemoveTlc(RemoveTlcInfo {
            tlc_id: TLCId::Offered(remove_tlc.tlc_id),
            channel_id: remove_tlc.channel_id,
            reason: remove_tlc.reason.clone(),
        });
        state.tlc_state.add_remote_tlc(tlc_kind.clone());
        Ok(())
    }

    async fn apply_remove_tlc_operation(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        remove_tlc: RemoveTlcInfo,
    ) -> Result<(), ProcessingChannelError> {
        let channel_id = state.get_id();
        let remove_reason = remove_tlc.reason.clone();
        let tlc_info = state
            .remove_tlc_with_reason(remove_tlc.tlc_id, &remove_reason)
            .expect("expect remove tlc successfully");
        if let (
            Some(ref udt_type_script),
            RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }),
        ) = (state.funding_udt_type_script.clone(), &remove_reason)
        {
            let mut tlc = tlc_info.clone();
            tlc.payment_preimage = Some(*payment_preimage);
            self.subscribers
                .settled_tlcs_subscribers
                .send(TlcNotification {
                    tlc,
                    channel_id,
                    script: udt_type_script.clone(),
                });
        }
        if tlc_info.previous_tlc.is_none() {
            // only the original sender of the TLC should send `TlcRemoveReceived` event
            // because only the original sender cares about the TLC event to settle the payment
            self.network
                .send_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::TlcRemoveReceived(
                        tlc_info.payment_hash,
                        remove_reason.clone(),
                    ),
                ))
                .expect("myself alive");
        } else {
            // relay RemoveTlc to previous channel if needed
            self.try_to_relay_remove_tlc(myself, state, &tlc_info, remove_reason)
                .await;
        }
        Ok(())
    }

    async fn handle_forward_onion_packet(
        &self,
        state: &mut ChannelActorState,
        payment_hash: Hash256,
        peeled_onion_packet: PeeledPaymentOnionPacket,
        added_tlc_id: u64,
    ) -> Result<(), ProcessingChannelError> {
        let (send, recv) = oneshot::channel::<Result<u64, TlcErr>>();
        let rpc_reply = RpcReplyPort::from(send);
        self.network
            .send_message(NetworkActorMessage::Command(
                NetworkActorCommand::SendPaymentOnionPacket(
                    SendOnionPacketCommand {
                        peeled_onion_packet,
                        previous_tlc: Some((state.get_id(), added_tlc_id)),
                        payment_hash,
                    },
                    rpc_reply,
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        // If we failed to forward the onion packet, we should remove the tlc.
        if let Err(res) = recv.await.expect("expect command replied") {
            return Err(ProcessingChannelError::TlcForwardingError(res));
        }
        Ok(())
    }

    pub fn handle_commitment_signed_command(
        &self,
        state: &mut ChannelActorState,
    ) -> ProcessingChannelResult {
        let flags = match state.state {
            ChannelState::CollaboratingFundingTx(flags)
                if !flags.contains(CollaboratingFundingTxFlags::COLLABRATION_COMPLETED) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed command in state {:?}, as collaboration is not completed yet.",
                    &state.state
                )));
            }
            ChannelState::CollaboratingFundingTx(_) => {
                CommitmentSignedFlags::SigningCommitment(SigningCommitmentFlags::empty())
            }
            ChannelState::SigningCommitment(flags)
                if flags.contains(SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed command in state {:?}, as we have already sent our commitment_signed message.",
                    &state.state
                )));
            }
            ChannelState::SigningCommitment(flags) => {
                CommitmentSignedFlags::SigningCommitment(flags)
            }
            ChannelState::ChannelReady() => CommitmentSignedFlags::ChannelReady(),
            ChannelState::ShuttingDown(flags) => {
                if flags.contains(ShuttingDownFlags::AWAITING_PENDING_TLCS) {
                    CommitmentSignedFlags::PendingShutdown()
                } else {
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Unable to process commitment_signed message in shutdowning state with flags {:?}",
                        &flags
                    )));
                }
            }
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to send commitment signed message in state {:?}",
                    &state.state
                )));
            }
        };
        let (funding_tx_partial_signature, commitment_tx_partial_signature) =
            state.build_and_sign_commitment_tx()?;
        let commitment_signed = CommitmentSigned {
            channel_id: state.get_id(),
            funding_tx_partial_signature,
            commitment_tx_partial_signature,
            next_local_nonce: state.get_next_local_nonce(),
        };
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                    state.get_remote_peer_id(),
                    FiberMessage::commitment_signed(commitment_signed),
                )),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        state.save_remote_nonce_for_raa();

        match flags {
            CommitmentSignedFlags::SigningCommitment(flags) => {
                let flags = flags | SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT;
                state.update_state(ChannelState::SigningCommitment(flags));
                state.maybe_transition_to_tx_signatures(flags, &self.network)?;
            }
            CommitmentSignedFlags::ChannelReady() => {}
            CommitmentSignedFlags::PendingShutdown() => {
                state.maybe_transition_to_shutdown(&self.network)?;
            }
        }
        Ok(())
    }

    pub fn handle_add_tlc_command(
        &self,
        state: &mut ChannelActorState,
        command: AddTlcCommand,
    ) -> Result<u64, ProcessingChannelError> {
        state.check_for_tlc_update(Some(command.amount), true, true)?;
        state.check_tlc_expiry(command.expiry)?;
        let tlc = state.create_outbounding_tlc(command.clone());
        state.check_insert_tlc(&tlc)?;
        state.tlc_state.add_local_tlc(TlcKind::AddTlc(tlc.clone()));
        state.increment_next_offered_tlc_id();

        let add_tlc = AddTlc {
            channel_id: state.get_id(),
            tlc_id: tlc.tlc_id.into(),
            amount: command.amount,
            payment_hash: command.payment_hash,
            expiry: command.expiry,
            hash_algorithm: command.hash_algorithm,
            onion_packet: command.onion_packet,
        };

        // Send tlc update message to peer.
        let msg =
            FiberMessageWithPeerId::new(state.get_remote_peer_id(), FiberMessage::add_tlc(add_tlc));

        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(msg),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        self.handle_commitment_signed_command(state)?;
        state.tlc_state.set_waiting_ack(true);
        Ok(tlc.tlc_id.into())
    }

    pub fn handle_remove_tlc_command(
        &self,
        state: &mut ChannelActorState,
        command: RemoveTlcCommand,
    ) -> ProcessingChannelResult {
        state.check_for_tlc_update(None, true, false)?;
        state.check_remove_tlc_with_reason(TLCId::Received(command.id), &command.reason)?;
        let tlc_kind = TlcKind::RemoveTlc(RemoveTlcInfo {
            channel_id: state.get_id(),
            tlc_id: TLCId::Received(command.id),
            reason: command.reason.clone(),
        });
        state.tlc_state.add_local_tlc(tlc_kind.clone());
        let msg = FiberMessageWithPeerId::new(
            state.get_remote_peer_id(),
            FiberMessage::remove_tlc(RemoveTlc {
                channel_id: state.get_id(),
                tlc_id: command.id,
                reason: command.reason,
            }),
        );
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(msg),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        state.maybe_transition_to_shutdown(&self.network)?;
        self.handle_commitment_signed_command(state)?;
        state.tlc_state.set_waiting_ack(true);
        Ok(())
    }

    pub fn handle_shutdown_command(
        &self,
        state: &mut ChannelActorState,
        command: ShutdownCommand,
    ) -> ProcessingChannelResult {
        debug!("Handling shutdown command: {:?}", &command);
        if command.force {
            match state.state {
                ChannelState::ChannelReady() => {
                    debug!("Handling force shutdown command in ChannelReady state");
                }
                ChannelState::ShuttingDown(flags) => {
                    debug!(
                        "Handling force shutdown command in ShuttingDown state, flags: {:?}",
                        &flags
                    );
                }
                _ => {
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Handling force shutdown command invalid state {:?}",
                        &state.state
                    )));
                }
            };

            let transaction = state
                .latest_commitment_transaction
                .clone()
                .expect("latest_commitment_transaction should exist when channel is in ChannelReady of ShuttingDown state");
            self.network
                .send_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::CommitmentTransactionPending(transaction, state.get_id()),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);

            state.update_state(ChannelState::ShuttingDown(
                ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION,
            ));
            return Ok(());
        }

        let flags = match state.state {
            ChannelState::ChannelReady() => {
                debug!("Handling shutdown command in ChannelReady state");
                ShuttingDownFlags::empty()
            }
            _ => {
                debug!("Handling shutdown command in state {:?}", &state.state);
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Trying to send shutdown message while in invalid state {:?}",
                    &state.state
                )));
            }
        };

        state.check_shutdown_fee_rate(command.fee_rate, &command.close_script)?;
        self.network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                    self.get_remote_peer_id(),
                    FiberMessage::shutdown(Shutdown {
                        channel_id: state.get_id(),
                        close_script: command.close_script.clone(),
                        fee_rate: command.fee_rate,
                    }),
                )),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        let shutdown_info = ShutdownInfo {
            close_script: command.close_script,
            fee_rate: command.fee_rate.as_u64(),
            signature: None,
        };
        state.local_shutdown_info = Some(shutdown_info);
        state.update_state(ChannelState::ShuttingDown(
            flags | ShuttingDownFlags::OUR_SHUTDOWN_SENT,
        ));
        debug!(
            "Channel state updated to {:?} after processing shutdown command",
            &state.state
        );

        state.maybe_transition_to_shutdown(&self.network)
    }

    pub async fn handle_update_command(
        &self,
        state: &mut ChannelActorState,
        command: UpdateCommand,
    ) -> ProcessingChannelResult {
        if !state.is_public() {
            return Err(ProcessingChannelError::InvalidState(
                "Only public channel can be updated".to_string(),
            ));
        }

        let UpdateCommand {
            enabled,
            tlc_expiry_delta,
            tlc_minimum_value,
            tlc_fee_proportional_millionths,
        } = command;

        let mut updated = false;

        if let Some(enabled) = enabled {
            updated |= state.update_our_enabled(enabled);
        }

        if let Some(delta) = tlc_expiry_delta {
            if delta < MIN_TLC_EXPIRY_DELTA {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "TLC expiry delta is too small, expect larger than {}",
                    MIN_TLC_EXPIRY_DELTA
                )));
            }
            updated |= state.update_our_tlc_expiry_delta(delta);
        }

        if let Some(value) = tlc_minimum_value {
            updated |= state.update_our_tlc_min_value(value);
        }

        if let Some(fee) = tlc_fee_proportional_millionths {
            updated |= state.update_our_tlc_fee_proportional_millionths(fee);
        }

        if updated {
            state
                .generate_and_broadcast_channel_update(&self.network)
                .await;
        }

        Ok(())
    }

    pub async fn register_retryable_tlc_remove(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        tlc_id: TLCId,
        reason: RemoveTlcReason,
    ) {
        state.tlc_state.set_tlc_pending_remove(tlc_id, reason);
        self.check_and_apply_retryable_remove_tlcs(myself, state)
            .await;
    }

    pub async fn register_retryable_relay_tlc_remove(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        tlc_id: u64,
        channel_id: Hash256,
        reason: RemoveTlcReason,
    ) {
        state
            .tlc_state
            .insert_relay_tlc_remove(channel_id, tlc_id, reason);
        self.check_and_apply_retryable_remove_tlcs(myself, state)
            .await;
    }

    pub async fn check_and_apply_retryable_remove_tlcs(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
    ) {
        let pending_removes = state.tlc_state.get_pending_remove();
        for retryable_remove in pending_removes.into_iter() {
            match retryable_remove {
                RetryableRemoveTlc::RemoveTlc(tlc_id, ref reason) => {
                    let command = RemoveTlcCommand {
                        id: tlc_id.into(),
                        reason: reason.clone(),
                    };

                    match self.handle_remove_tlc_command(state, command) {
                        Ok(_) | Err(ProcessingChannelError::RepeatedProcessing(_)) => {
                            state.tlc_state.remove_pending_remove_tlc(&retryable_remove);
                        }
                        Err(ProcessingChannelError::WaitingTlcAck) => {
                            error!(
                                "Failed to remove tlc: {:?} because of WaitingTlcAck, retry it later",
                                &retryable_remove
                            );
                        }
                        Err(err) => {
                            error!(
                                "Failed to remove tlc: {:?} with reason: {:?}, will not retry",
                                &retryable_remove, err
                            );
                            state.tlc_state.remove_pending_remove_tlc(&retryable_remove);
                        }
                    }
                }
                RetryableRemoveTlc::RelayRemoveTlc(channel_id, tlc_id, ref reason) => {
                    let (send, recv) = oneshot::channel::<Result<(), String>>();
                    let port = RpcReplyPort::from(send);
                    self.network
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                                channel_id: channel_id,
                                command: ChannelCommand::RemoveTlc(
                                    RemoveTlcCommand {
                                        id: tlc_id,
                                        reason: reason.clone(),
                                    },
                                    port,
                                ),
                            }),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    if let Ok(res) = recv.await {
                        match res {
                            Ok(_) => {
                                state.tlc_state.remove_pending_remove_tlc(&retryable_remove);
                            }
                            Err(err) if err.contains("WaitingTlcAck") => {
                                error!(
                                "Failed to relay remove tlc: {:?} because of WaitingTlcAck, retry it later",
                                &retryable_remove
                            );
                            }
                            Err(err) => {
                                error!(
                                "Failed to relay remove tlc: {:?} with reason: {:?}, will not retry",
                                &retryable_remove, err
                            );
                                state.tlc_state.remove_pending_remove_tlc(&retryable_remove);
                            }
                        }
                    }
                }
            }
        }
        // If there are more pending removes, we will retry it later
        if !state.tlc_state.get_pending_remove().is_empty() {
            myself.send_after(AUTO_SETDOWN_TLC_INTERVAL, || {
                ChannelActorMessage::Event(ChannelEvent::CheckTlcSetdown)
            });
        }
    }

    // This is the dual of `handle_tx_collaboration_msg`. Any logic error here is likely
    // to present in the other function as well.
    pub fn handle_tx_collaboration_command(
        &self,
        state: &mut ChannelActorState,
        command: TxCollaborationCommand,
    ) -> Result<(), ProcessingChannelError> {
        debug!("Handling tx collaboration command: {:?}", &command);
        let is_complete_command = matches!(command, TxCollaborationCommand::TxComplete());
        let is_waiting_for_remote = match state.state {
            ChannelState::CollaboratingFundingTx(flags) => {
                flags.contains(CollaboratingFundingTxFlags::AWAITING_REMOTE_TX_COLLABORATION_MSG)
            }
            _ => false,
        };

        // We first exclude below cases that are invalid for tx collaboration,
        // and then process the commands.
        let flags = match state.state {
            ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT)
                if state.is_acceptor =>
            {
                return Err(ProcessingChannelError::InvalidState(
                    "Acceptor tries to start sending tx collaboration message".to_string(),
                ));
            }
            ChannelState::NegotiatingFunding(_) => {
                debug!("Beginning processing tx collaboration command, and transitioning from {:?} to CollaboratingFundingTx state", state.state);
                state.state =
                    ChannelState::CollaboratingFundingTx(CollaboratingFundingTxFlags::empty());
                CollaboratingFundingTxFlags::empty()
            }
            ChannelState::CollaboratingFundingTx(_)
                if !is_complete_command && is_waiting_for_remote =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Trying to process command {:?} while in {:?} (should only send non-complete message after received response from peer)",
                    &command, state.state
                )));
            }
            ChannelState::CollaboratingFundingTx(flags) => {
                debug!(
                    "Processing tx collaboration command {:?} for state {:?}",
                    &command, &state.state
                );
                flags
            }
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Invalid tx collaboration command {:?} for state {:?}",
                    &command, state.state
                )));
            }
        };

        // TODO: Note that we may deadlock here if send_tx_collaboration_command does successfully send the message,
        // as in that case both us and the remote are waiting for each other to send the message.
        match command {
            TxCollaborationCommand::TxUpdate(tx_update) => {
                let fiber_message = FiberMessage::tx_update(TxUpdate {
                    channel_id: state.get_id(),
                    tx: tx_update.transaction.clone(),
                });
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                            state.get_remote_peer_id(),
                            fiber_message,
                        )),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                state.update_state(ChannelState::CollaboratingFundingTx(
                    CollaboratingFundingTxFlags::AWAITING_REMOTE_TX_COLLABORATION_MSG,
                ));
                state.funding_tx = Some(tx_update.transaction.clone());
                state.maybe_complete_tx_collaboration(tx_update.transaction, &self.network)?;
            }
            TxCollaborationCommand::TxComplete() => {
                state.check_tx_complete_preconditions()?;
                let commitment_tx_partial_signature = state.build_init_commitment_tx_signature()?;
                let fiber_message = FiberMessage::tx_complete(TxComplete {
                    channel_id: state.get_id(),
                    commitment_tx_partial_signature,
                });
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                            state.get_remote_peer_id(),
                            fiber_message,
                        )),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                state.update_state(ChannelState::CollaboratingFundingTx(
                    flags | CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT,
                ));
            }
        }

        Ok(())
    }

    pub async fn handle_command(
        &self,
        state: &mut ChannelActorState,
        command: ChannelCommand,
    ) -> Result<(), ProcessingChannelError> {
        match command {
            ChannelCommand::TxCollaborationCommand(tx_collaboration_command) => {
                self.handle_tx_collaboration_command(state, tx_collaboration_command)
            }
            ChannelCommand::CommitmentSigned() => self.handle_commitment_signed_command(state),
            ChannelCommand::AddTlc(command, reply) => {
                match self.handle_add_tlc_command(state, command) {
                    Ok(tlc_id) => {
                        let _ = reply.send(Ok(AddTlcResponse { tlc_id }));
                        Ok(())
                    }
                    Err(err) => {
                        debug!("Error processing AddTlc command: {:?}", &err);
                        let tlc_err = self.get_tlc_error(state, &err).await;
                        let _ = reply.send(Err(tlc_err));
                        Err(err)
                    }
                }
            }
            ChannelCommand::RemoveTlc(command, reply) => {
                match self.handle_remove_tlc_command(state, command) {
                    Ok(_) => {
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err.to_string()));
                        Err(err)
                    }
                }
            }
            ChannelCommand::Shutdown(command, reply) => {
                match self.handle_shutdown_command(state, command) {
                    Ok(_) => {
                        debug!("Shutdown command processed successfully");
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(err) => {
                        debug!("Error processing shutdown command: {:?}", &err);
                        let _ = reply.send(Err(err.to_string()));
                        Err(err)
                    }
                }
            }
            ChannelCommand::Update(command, reply) => {
                match self.handle_update_command(state, command).await {
                    Ok(_) => {
                        debug!("Update command processed successfully");
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(err) => {
                        debug!("Error processing update command: {:?}", &err);
                        let _ = reply.send(Err(err.to_string()));
                        Err(err)
                    }
                }
            }
            #[cfg(test)]
            ChannelCommand::ReloadState() => {
                *state = self
                    .store
                    .get_channel_actor_state(&state.get_id())
                    .expect("load channel state failed");
                Ok(())
            }
        }
    }

    pub async fn handle_event(
        &self,
        myself: &ActorRef<ChannelActorMessage>,
        state: &mut ChannelActorState,
        event: ChannelEvent,
    ) -> Result<(), ProcessingChannelError> {
        match event {
            ChannelEvent::FundingTransactionConfirmed(block_number, tx_index) => {
                debug!("Funding transaction confirmed");
                let flags = match state.state {
                    ChannelState::AwaitingChannelReady(flags) => flags,
                    ChannelState::AwaitingTxSignatures(f)
                        if f.contains(AwaitingTxSignaturesFlags::TX_SIGNATURES_SENT) =>
                    {
                        AwaitingChannelReadyFlags::empty()
                    }
                    _ => {
                        return Err(ProcessingChannelError::InvalidState(format!(
                            "Expecting funding transaction confirmed event in state AwaitingChannelReady or after TX_SIGNATURES_SENT, but got state {:?}", &state.state)));
                    }
                };
                state.funding_tx_confirmed_at = Some((block_number, tx_index));
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                            state.get_remote_peer_id(),
                            FiberMessage::channel_ready(ChannelReady {
                                channel_id: state.get_id(),
                            }),
                        )),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                let flags = flags | AwaitingChannelReadyFlags::OUR_CHANNEL_READY;
                state.update_state(ChannelState::AwaitingChannelReady(flags));
                state.maybe_channel_is_ready(&self.network).await;
            }
            ChannelEvent::CommitmentTransactionConfirmed => {
                match state.state {
                    ChannelState::ShuttingDown(flags)
                        if flags.contains(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION) => {}
                    _ => {
                        return Err(ProcessingChannelError::InvalidState(format!(
                            "Expecting commitment transaction confirmed event in state ShuttingDown, but got state {:?}", &state.state)
                        ));
                    }
                };
                state.update_state(ChannelState::Closed(CloseFlags::UNCOOPERATIVE));
                debug!("Channel closed with uncooperative close");
            }
            ChannelEvent::CheckTlcSetdown => {
                self.check_and_apply_retryable_remove_tlcs(myself, state)
                    .await;
            }
            ChannelEvent::PeerDisconnected => {
                myself.stop(Some("PeerDisconnected".to_string()));
            }
            ChannelEvent::ClosingTransactionConfirmed => {
                // Broadcast the channel update message which disables the channel.
                let update = state.generate_disabled_channel_update(&self.network).await;

                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::BroadcastMessages(vec![
                            BroadcastMessage::ChannelUpdate(update),
                        ]),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                myself.stop(Some("ChannelClosed".to_string()));
            }
        }
        Ok(())
    }

    fn get_invoice_status(&self, invoice: &CkbInvoice) -> CkbInvoiceStatus {
        match self
            .store
            .get_invoice_status(&invoice.payment_hash())
            .expect("no invoice status found")
        {
            CkbInvoiceStatus::Open if invoice.is_expired() => CkbInvoiceStatus::Expired,
            status => status,
        }
    }

    async fn peel_onion_packet(
        &self,
        onion_packet: PaymentOnionPacket,
        payment_hash: Hash256,
    ) -> Result<PeeledPaymentOnionPacket, ProcessingChannelError> {
        call!(self.network, |tx| NetworkActorMessage::Command(
            NetworkActorCommand::PeelPaymentOnionPacket(onion_packet, payment_hash, tx)
        ))
        .expect(ASSUME_NETWORK_ACTOR_ALIVE)
        .map_err(|err| ProcessingChannelError::PeelingOnionPacketError(err))
    }
}

#[rasync_trait]
impl<S> Actor for ChannelActor<S>
where
    S: ChannelActorStateStore + InvoiceStore + Send + Sync + 'static,
{
    type Msg = ChannelActorMessage;
    type State = ChannelActorState;
    type Arguments = ChannelInitializationParameter;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        // startup the event processing
        match args {
            ChannelInitializationParameter::AcceptChannel(AcceptChannelParameter {
                funding_amount: local_funding_amount,
                reserved_ckb_amount: local_reserved_ckb_amount,
                shutdown_script: local_shutdown_script,
                public_channel_info,
                seed,
                open_channel,
                channel_id_sender,
                max_tlc_number_in_flight,
                max_tlc_value_in_flight,
            }) => {
                let peer_id = self.get_remote_peer_id();
                debug!(
                    "Accepting channel {:?} to peer {:?}",
                    &open_channel, &peer_id
                );

                let counterpart_pubkeys = (&open_channel).into();
                let public = open_channel.is_public();
                let OpenChannel {
                    channel_id,
                    chain_hash,
                    commitment_fee_rate,
                    commitment_delay_epoch,
                    funding_fee_rate,
                    funding_udt_type_script,
                    funding_amount,
                    shutdown_script,
                    reserved_ckb_amount,
                    first_per_commitment_point,
                    second_per_commitment_point,
                    next_local_nonce,
                    max_tlc_value_in_flight: remote_max_tlc_value_in_flight,
                    max_tlc_number_in_flight: remote_max_tlc_number_in_flight,
                    channel_announcement_nonce,
                    ..
                } = &open_channel;

                if *chain_hash != get_chain_hash() {
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(format!(
                        "Invalid chain hash {:?}",
                        chain_hash
                    ))));
                }

                // TODO: we may reject the channel opening request here
                // if the peer want to open a public channel, but we don't want to.
                if public && (channel_announcement_nonce.is_none() || public_channel_info.is_none())
                {
                    return Err(Box::new(ProcessingChannelError::InvalidParameter(
                        "Public channel should have channel announcement nonce and public channel info".to_string(),
                    )));
                }

                let mut state = ChannelActorState::new_inbound_channel(
                    *channel_id,
                    public_channel_info,
                    local_funding_amount,
                    local_reserved_ckb_amount,
                    *commitment_fee_rate,
                    *commitment_delay_epoch,
                    *funding_fee_rate,
                    funding_udt_type_script.clone(),
                    &seed,
                    self.get_local_pubkey(),
                    self.get_remote_pubkey(),
                    local_shutdown_script.clone(),
                    shutdown_script.clone(),
                    *funding_amount,
                    *reserved_ckb_amount,
                    counterpart_pubkeys,
                    next_local_nonce.clone(),
                    channel_announcement_nonce.clone(),
                    *first_per_commitment_point,
                    *second_per_commitment_point,
                    *remote_max_tlc_value_in_flight,
                    *remote_max_tlc_number_in_flight,
                    max_tlc_number_in_flight,
                    max_tlc_value_in_flight,
                );
                state.check_accept_channel_parameters()?;

                let commitment_number = INITIAL_COMMITMENT_NUMBER;

                let channel_announcement_nonce = if public {
                    Some(state.get_channel_announcement_musig2_pubnonce())
                } else {
                    None
                };
                let accept_channel = AcceptChannel {
                    channel_id: *channel_id,
                    funding_amount: local_funding_amount,
                    shutdown_script: local_shutdown_script,
                    reserved_ckb_amount: local_reserved_ckb_amount,
                    max_tlc_value_in_flight,
                    max_tlc_number_in_flight,
                    funding_pubkey: state.signer.funding_key.pubkey(),
                    tlc_basepoint: state.signer.tlc_base_key.pubkey(),
                    first_per_commitment_point: state
                        .signer
                        .get_commitment_point(commitment_number),
                    second_per_commitment_point: state
                        .signer
                        .get_commitment_point(commitment_number + 1),
                    channel_announcement_nonce,
                    next_local_nonce: state.get_local_musig2_pubnonce(),
                };

                let command = FiberMessageWithPeerId::new(
                    peer_id,
                    FiberMessage::accept_channel(accept_channel),
                );
                // TODO: maybe we should not use try_send here.
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(command),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                state.update_state(ChannelState::NegotiatingFunding(
                    NegotiatingFundingFlags::INIT_SENT,
                ));
                if let Some(sender) = channel_id_sender {
                    sender.send(state.get_id()).expect("Receive not dropped");
                }
                Ok(state)
            }
            ChannelInitializationParameter::OpenChannel(OpenChannelParameter {
                funding_amount,
                seed,
                public_channel_info,
                funding_udt_type_script,
                shutdown_script,
                channel_id_sender,
                commitment_fee_rate,
                commitment_delay_epoch,
                funding_fee_rate,
                max_tlc_number_in_flight,
                max_tlc_value_in_flight,
            }) => {
                let public = public_channel_info.is_some();
                let peer_id = self.get_remote_peer_id();
                info!("Trying to open a channel to {:?}", &peer_id);

                let commitment_fee_rate =
                    commitment_fee_rate.unwrap_or(DEFAULT_COMMITMENT_FEE_RATE);
                let funding_fee_rate = funding_fee_rate.unwrap_or(DEFAULT_FEE_RATE);

                let (to_local_amount, reserved_ckb_amount) = get_funding_and_reserved_amount(
                    funding_amount,
                    &shutdown_script,
                    &funding_udt_type_script,
                )?;

                let mut channel = ChannelActorState::new_outbound_channel(
                    public_channel_info,
                    &seed,
                    self.get_local_pubkey(),
                    self.get_remote_pubkey(),
                    to_local_amount,
                    reserved_ckb_amount,
                    commitment_fee_rate,
                    commitment_delay_epoch
                        .unwrap_or(EpochNumberWithFraction::new(
                            DEFAULT_COMMITMENT_DELAY_EPOCHS,
                            0,
                            1,
                        ))
                        .full_value(),
                    funding_fee_rate,
                    funding_udt_type_script.clone(),
                    shutdown_script.clone(),
                    max_tlc_value_in_flight,
                    max_tlc_number_in_flight,
                );

                channel.check_open_channel_parameters()?;

                let channel_flags = if public {
                    ChannelFlags::PUBLIC
                } else {
                    ChannelFlags::empty()
                };
                let channel_announcement_nonce = if public {
                    Some(channel.get_channel_announcement_musig2_pubnonce())
                } else {
                    None
                };
                let commitment_number = INITIAL_COMMITMENT_NUMBER;
                let message = FiberMessage::ChannelInitialization(OpenChannel {
                    chain_hash: get_chain_hash(),
                    channel_id: channel.get_id(),
                    funding_udt_type_script,
                    funding_amount: channel.to_local_amount,
                    shutdown_script,
                    reserved_ckb_amount: channel.local_reserved_ckb_amount,
                    funding_fee_rate,
                    commitment_fee_rate,
                    commitment_delay_epoch: channel.commitment_delay_epoch,
                    max_tlc_value_in_flight: channel.local_constraints.max_tlc_value_in_flight,
                    max_tlc_number_in_flight: channel.local_constraints.max_tlc_number_in_flight,
                    channel_flags,
                    first_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number),
                    second_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number + 1),
                    funding_pubkey: channel.get_local_channel_public_keys().funding_pubkey,
                    tlc_basepoint: channel.get_local_channel_public_keys().tlc_base_key,
                    next_local_nonce: channel.get_local_musig2_pubnonce(),
                    channel_announcement_nonce,
                });

                debug!(
                    "Created OpenChannel message to {:?}: {:?}",
                    &peer_id, &message
                );
                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId {
                            peer_id: peer_id.clone(),
                            message,
                        }),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                // TODO: note that we can't actually guarantee that this OpenChannel message is sent here.
                // It is even possible that the peer_id is bogus, and we can't send a message to it.
                // We need some book-keeping service to remove all the OUR_INIT_SENT channels.
                channel.update_state(ChannelState::NegotiatingFunding(
                    NegotiatingFundingFlags::OUR_INIT_SENT,
                ));
                debug!(
                    "Channel to peer {:?} with id {:?} created",
                    &peer_id,
                    &channel.get_id()
                );

                channel_id_sender
                    .send(channel.get_id())
                    .expect("Receive not dropped");
                Ok(channel)
            }
            ChannelInitializationParameter::ReestablishChannel(channel_id) => {
                let mut channel = self
                    .store
                    .get_channel_actor_state(&channel_id)
                    .expect("channel should exist");
                channel.reestablishing = true;

                let reestablish_channel = ReestablishChannel {
                    channel_id,
                    local_commitment_number: channel.get_current_commitment_number(true),
                    remote_commitment_number: channel.get_current_commitment_number(false),
                };

                let command = FiberMessageWithPeerId::new(
                    self.get_remote_peer_id(),
                    FiberMessage::reestablish_channel(reestablish_channel),
                );

                self.network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendFiberMessage(command),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                // If the channel is already ready, we should notify the network actor.
                // so that we update the network.outpoint_channel_map
                if matches!(channel.state, ChannelState::ChannelReady()) {
                    self.network
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::ChannelReady(
                                channel.get_id(),
                                channel.get_remote_peer_id(),
                                channel.must_get_funding_transaction_outpoint(),
                            ),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                }
                Ok(channel)
            }
        }
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        trace!(
            "Channel actor processing message: id: {:?}, state: {:?}, message: {:?}",
            &state.get_id(),
            &state.state,
            message,
        );

        match message {
            ChannelActorMessage::PeerMessage(message) => {
                if let Err(error) = self.handle_peer_message(&myself, state, message).await {
                    error!("Error while processing channel message: {:?}", error);
                    #[cfg(debug_assertions)]
                    self.network
                        .clone()
                        .send_message(NetworkActorMessage::new_notification(
                            NetworkServiceEvent::DebugEvent(DebugEvent::Common(format!(
                                "{:?}",
                                error
                            ))),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                }
            }
            ChannelActorMessage::Command(command) => {
                if let Err(err) = self.handle_command(state, command).await {
                    error!("Error while processing channel command: {:?}", err);
                }
            }
            ChannelActorMessage::Event(e) => {
                if let Err(err) = self.handle_event(&myself, state, e).await {
                    error!("Error while processing channel event: {:?}", err);
                }
            }
        }

        self.store.insert_channel_actor_state(state.clone());
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitmentNumbers {
    pub local: u64,
    pub remote: u64,
}

impl Default for CommitmentNumbers {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitmentNumbers {
    pub fn new() -> Self {
        Self {
            local: INITIAL_COMMITMENT_NUMBER,
            remote: INITIAL_COMMITMENT_NUMBER,
        }
    }

    pub fn get_local(&self) -> u64 {
        self.local
    }

    pub fn get_remote(&self) -> u64 {
        self.remote
    }

    pub fn increment_local(&mut self) {
        self.local += 1;
    }

    pub fn increment_remote(&mut self) {
        self.remote += 1;
    }

    pub fn flip(&self) -> Self {
        Self {
            local: self.remote,
            remote: self.local,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
pub enum TLCId {
    Offered(u64),
    Received(u64),
}

impl From<TLCId> for u64 {
    fn from(id: TLCId) -> u64 {
        match id {
            TLCId::Offered(id) => id,
            TLCId::Received(id) => id,
        }
    }
}

impl TLCId {
    pub fn is_offered(&self) -> bool {
        matches!(self, TLCId::Offered(_))
    }

    pub fn is_received(&self) -> bool {
        !self.is_offered()
    }

    pub fn flip(&self) -> Self {
        match self {
            TLCId::Offered(id) => TLCId::Received(*id),
            TLCId::Received(id) => TLCId::Offered(*id),
        }
    }

    pub fn flip_mut(&mut self) {
        *self = self.flip();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum TlcKind {
    AddTlc(AddTlcInfo),
    RemoveTlc(RemoveTlcInfo),
}

impl TlcKind {
    pub fn log(&self) -> String {
        match self {
            TlcKind::AddTlc(add_tlc) => {
                format!("{:?}", &add_tlc.tlc_id)
            }
            TlcKind::RemoveTlc(remove_tlc) => {
                format!("RemoveTlc({:?})", &remove_tlc.tlc_id)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct AddTlcInfo {
    pub channel_id: Hash256,
    pub tlc_id: TLCId,
    pub amount: u128,
    pub payment_hash: Hash256,
    pub expiry: u64,
    pub hash_algorithm: HashAlgorithm,
    // the onion packet for multi-hop payment
    pub onion_packet: Option<PaymentOnionPacket>,
    /// Shared secret used in forwarding.
    ///
    /// Save it to backward errors. Use all zeros when no shared secrets are available.
    pub shared_secret: [u8; 32],
    pub created_at: CommitmentNumbers,
    pub removed_at: Option<(CommitmentNumbers, RemoveTlcReason)>,
    pub payment_preimage: Option<Hash256>,

    /// Note: `previous_tlc` is used to track the tlc chain for a multi-tlc payment,
    ///       we need to know previous when removing tlc backwardly.
    ///
    /// Node A ---------> Node B ------------> Node C ----------> Node D
    ///  tlc_1 <---> (tlc_1) (tlc_2) <---> (tlc_2) (tlc_3) <----> tlc_3
    ///                ^^^^                 ^^^^
    ///
    pub previous_tlc: Option<(Hash256, TLCId)>,
}

impl AddTlcInfo {
    pub fn is_offered(&self) -> bool {
        self.tlc_id.is_offered()
    }

    pub fn is_received(&self) -> bool {
        !self.is_offered()
    }

    pub fn get_commitment_numbers(&self) -> CommitmentNumbers {
        self.created_at
    }

    pub fn flip_mut(&mut self) {
        self.tlc_id.flip_mut();
    }

    /// Get the value for the field `htlc_type` in commitment lock witness.
    /// - Lowest 1 bit: 0 if the tlc is offered by the remote party, 1 otherwise.
    /// - High 7 bits:
    ///     - 0: ckb hash
    ///     - 1: sha256
    pub fn get_htlc_type(&self) -> u8 {
        let offered_flag = if self.is_offered() { 0u8 } else { 1u8 };
        ((self.hash_algorithm as u8) << 1) + offered_flag
    }

    fn get_hash(&self) -> ShortHash {
        self.payment_hash.as_ref()[..20]
            .try_into()
            .expect("short hash from payment hash")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct RemoveTlcInfo {
    pub channel_id: Hash256,
    pub tlc_id: TLCId,
    pub reason: RemoveTlcReason,
}

impl TlcKind {
    pub fn tlc_id_u64(&self) -> u64 {
        match self {
            TlcKind::AddTlc(add_tlc) => add_tlc.tlc_id.into(),
            TlcKind::RemoveTlc(remove_tlc) => remove_tlc.tlc_id.into(),
        }
    }

    pub fn tlc_id(&self) -> TLCId {
        match self {
            TlcKind::AddTlc(info) => info.tlc_id,
            TlcKind::RemoveTlc(remove_tlc) => remove_tlc.tlc_id,
        }
    }

    pub fn is_offered(&self) -> bool {
        self.tlc_id().is_offered()
    }

    pub fn is_received(&self) -> bool {
        !self.is_offered()
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct PendingTlcs {
    tlcs: Vec<TlcKind>,
    committed_index: usize,
    next_tlc_id: u64,
}

impl PendingTlcs {
    pub fn new() -> Self {
        Self {
            tlcs: Vec::new(),
            committed_index: 0,
            next_tlc_id: 0,
        }
    }

    pub fn next_tlc_id(&self) -> u64 {
        self.next_tlc_id
    }

    pub fn increment_next_tlc_id(&mut self) {
        self.next_tlc_id += 1;
    }

    pub fn add_tlc_operation(&mut self, tlc_op: TlcKind) {
        self.tlcs.push(tlc_op);
    }

    pub fn get_staging_tlcs(&self) -> &[TlcKind] {
        &self.tlcs[self.committed_index..]
    }

    pub fn get_committed_tlcs(&self) -> &[TlcKind] {
        &self.tlcs[..self.committed_index]
    }

    pub fn get_committed_tlcs_mut(&mut self) -> &mut [TlcKind] {
        &mut self.tlcs[..self.committed_index]
    }

    pub fn tlcs(&self) -> &[TlcKind] {
        &self.tlcs
    }

    pub fn tlcs_mut(&mut self) -> &mut Vec<TlcKind> {
        &mut self.tlcs
    }

    pub fn push(&mut self, tlc: TlcKind) {
        assert!(!self.is_tlc_present(&tlc));
        self.tlcs.push(tlc);
    }

    fn is_tlc_present(&self, tlc: &TlcKind) -> bool {
        self.tlcs.iter().any(|t| match (t, tlc) {
            (TlcKind::AddTlc(info1), TlcKind::AddTlc(info2)) => info1.tlc_id == info2.tlc_id,
            (TlcKind::RemoveTlc(info1), TlcKind::RemoveTlc(info2)) => info1.tlc_id == info2.tlc_id,
            _ => false,
        })
    }

    pub fn commit_tlcs(&mut self, committed_tlcs: &[TlcKind]) -> Vec<TlcKind> {
        let staging_tlcs = self.get_staging_tlcs().to_vec();
        for tlc in committed_tlcs {
            if !self.is_tlc_present(tlc) {
                self.tlcs.push(tlc.clone());
            }
        }
        self.committed_index = self.tlcs.len();
        return staging_tlcs;
    }

    pub fn get_mut(&mut self, tlc_id: &TLCId) -> Option<&mut AddTlcInfo> {
        self.tlcs.iter_mut().find_map(|tlc| match tlc {
            TlcKind::AddTlc(info) if info.tlc_id == *tlc_id => Some(info),
            _ => None,
        })
    }

    pub fn drop_remove_tlc(&mut self, tlc_id: &TLCId) {
        self.tlcs.retain(|tlc| match tlc {
            TlcKind::RemoveTlc(info) => info.tlc_id != *tlc_id,
            _ => true,
        });
        self.committed_index = self.tlcs.len();
    }

    pub fn shrink_removed_tlc(&mut self) {
        assert_eq!(self.committed_index, self.tlcs.len());
        let new_committed_index = self
            .get_committed_tlcs()
            .iter()
            .filter(|tlc| match tlc {
                TlcKind::AddTlc(info) => info.removed_at.is_none(),
                _ => true,
            })
            .count();
        self.tlcs.retain(|tlc| match tlc {
            TlcKind::AddTlc(info) => info.removed_at.is_none(),
            _ => true,
        });
        self.committed_index = new_committed_index;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum RetryableRemoveTlc {
    RemoveTlc(TLCId, RemoveTlcReason),
    RelayRemoveTlc(Hash256, u64, RemoveTlcReason),
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct TlcState {
    local_pending_tlcs: PendingTlcs,
    remote_pending_tlcs: PendingTlcs,
    // if the tlc is pending to be removed, the reason will be stored here
    // this will only used for retrying remove TLC
    retryable_remove_tlcs: Vec<RetryableRemoveTlc>,
    waiting_ack: bool,
}

impl TlcState {
    pub fn get_next_offering(&self) -> u64 {
        self.local_pending_tlcs.next_tlc_id()
    }

    pub fn get_next_received(&self) -> u64 {
        self.remote_pending_tlcs.next_tlc_id()
    }

    pub fn increment_offering(&mut self) {
        self.local_pending_tlcs.increment_next_tlc_id();
    }

    pub fn increment_received(&mut self) {
        self.remote_pending_tlcs.increment_next_tlc_id();
    }

    pub fn set_waiting_ack(&mut self, waiting_ack: bool) {
        self.waiting_ack = waiting_ack;
    }

    pub fn set_tlc_pending_remove(&mut self, tlc_id: TLCId, reason: RemoveTlcReason) {
        self.retryable_remove_tlcs
            .push(RetryableRemoveTlc::RemoveTlc(tlc_id, reason));
    }

    pub fn insert_relay_tlc_remove(
        &mut self,
        channel_id: Hash256,
        tlc_id: u64,
        reason: RemoveTlcReason,
    ) {
        self.retryable_remove_tlcs
            .push(RetryableRemoveTlc::RelayRemoveTlc(
                channel_id, tlc_id, reason,
            ));
    }

    pub fn get_pending_remove(&self) -> Vec<RetryableRemoveTlc> {
        self.retryable_remove_tlcs.clone()
    }

    pub fn remove_pending_remove_tlc(&mut self, retryable_remove: &RetryableRemoveTlc) {
        self.retryable_remove_tlcs
            .retain(|remove| remove != retryable_remove);
    }

    pub fn get(&self, id: &TLCId) -> Option<&AddTlcInfo> {
        match id {
            TLCId::Offered(_id) => {
                self.local_pending_tlcs
                    .tlcs()
                    .iter()
                    .find_map(|tlc| match tlc {
                        TlcKind::AddTlc(info) if info.tlc_id == *id => Some(info),
                        _ => None,
                    })
            }
            TLCId::Received(_id) => {
                self.remote_pending_tlcs
                    .tlcs()
                    .iter()
                    .find_map(|tlc| match tlc {
                        TlcKind::AddTlc(info) if info.tlc_id == *id => Some(info),
                        _ => None,
                    })
            }
        }
    }

    pub fn get_mut(&mut self, id: &TLCId) -> Option<&mut AddTlcInfo> {
        match id {
            TLCId::Offered(_id) => {
                self.local_pending_tlcs
                    .tlcs
                    .iter_mut()
                    .find_map(|tlc| match tlc {
                        TlcKind::AddTlc(info) if info.tlc_id == *id => Some(info),
                        _ => None,
                    })
            }
            TLCId::Received(_id) => {
                self.remote_pending_tlcs
                    .tlcs
                    .iter_mut()
                    .find_map(|tlc| match tlc {
                        TlcKind::AddTlc(info) if info.tlc_id == *id => Some(info),
                        _ => None,
                    })
            }
        }
    }

    pub fn add_local_tlc(&mut self, tlc_info: TlcKind) {
        self.local_pending_tlcs.push(tlc_info);
    }

    pub fn add_remote_tlc(&mut self, tlc_info: TlcKind) {
        self.remote_pending_tlcs.push(tlc_info);
    }

    pub fn next_local_tlc_id(&self) -> u64 {
        self.local_pending_tlcs.next_tlc_id()
    }

    pub fn increment_local_tlc_id(&mut self) {
        self.local_pending_tlcs.increment_next_tlc_id();
    }

    pub fn next_remote_tlc_id(&self) -> u64 {
        self.remote_pending_tlcs.next_tlc_id()
    }

    pub fn increment_remote_tlc_id(&mut self) {
        self.remote_pending_tlcs.increment_next_tlc_id();
    }

    fn unify_tlcs<'a>(
        &self,
        staging_tlcs: impl Iterator<Item = &'a TlcKind>,
        committed_tlcs: impl Iterator<Item = &'a TlcKind>,
    ) -> Vec<TlcKind> {
        let mut add_tlcs: BTreeMap<TLCId, TlcKind> = Default::default();
        let mut remove_tlcs = vec![];
        for tlc in committed_tlcs {
            match tlc {
                TlcKind::AddTlc(info) => {
                    if info.removed_at.is_none() {
                        let _ = add_tlcs.insert(tlc.tlc_id(), tlc.clone());
                    }
                }
                TlcKind::RemoveTlc(..) => {
                    unreachable!("RemoveTlc should not be in committed tlcs")
                }
            }
        }
        for tlc in staging_tlcs {
            match tlc {
                TlcKind::AddTlc(_info) => {
                    // If the tlc is already in the committed tlcs, we should not add it again.
                    // committed tlcs always have the latest tlc information
                    if add_tlcs.contains_key(&tlc.tlc_id()) {
                        continue;
                    }
                    let _ = add_tlcs.insert(tlc.tlc_id(), tlc.clone());
                }
                TlcKind::RemoveTlc(..) => remove_tlcs.push(tlc.clone()),
            }
        }
        for tlc in remove_tlcs {
            let tlc_id = tlc.tlc_id();
            let _ = add_tlcs.remove(&tlc_id);
        }
        add_tlcs.values().map(|tlc| tlc.clone()).collect()
    }

    pub fn get_tlcs_for_local(&self) -> Vec<TlcKind> {
        self.unify_tlcs(
            self.local_pending_tlcs.get_staging_tlcs().into_iter(),
            self.local_pending_tlcs
                .get_committed_tlcs()
                .into_iter()
                .chain(self.remote_pending_tlcs.get_committed_tlcs().into_iter()),
        )
    }

    pub fn get_tlcs_for_remote(&self) -> Vec<TlcKind> {
        self.unify_tlcs(
            self.remote_pending_tlcs.get_staging_tlcs().into_iter(),
            self.remote_pending_tlcs
                .get_committed_tlcs()
                .into_iter()
                .chain(self.local_pending_tlcs.get_committed_tlcs().into_iter()),
        )
    }

    pub fn get_tlcs_with(&self, local_commitment: bool) -> Vec<TlcKind> {
        if local_commitment {
            self.get_tlcs_for_local()
        } else {
            self.get_tlcs_for_remote()
        }
    }

    pub fn commit_local_tlcs(&mut self) -> Vec<TlcKind> {
        self.local_pending_tlcs
            .commit_tlcs(self.remote_pending_tlcs.get_committed_tlcs())
    }

    pub fn commit_remote_tlcs(&mut self) -> Vec<TlcKind> {
        self.remote_pending_tlcs
            .commit_tlcs(self.local_pending_tlcs.get_committed_tlcs())
    }

    fn filter_add_tlcs<'a, I>(tlcs: I) -> impl Iterator<Item = &'a AddTlcInfo>
    where
        I: Iterator<Item = &'a TlcKind>,
    {
        tlcs.filter_map(|tlc| match tlc {
            TlcKind::AddTlc(info) => {
                if info.removed_at.is_some() {
                    None
                } else {
                    Some((info.tlc_id, info))
                }
            }
            TlcKind::RemoveTlc(..) => None,
        })
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .map(|(_, v)| v)
    }

    pub fn all_commited_tlcs(&self) -> impl Iterator<Item = &AddTlcInfo> {
        Self::filter_add_tlcs(
            self.local_pending_tlcs
                .get_committed_tlcs()
                .into_iter()
                .chain(self.remote_pending_tlcs.get_committed_tlcs().into_iter()),
        )
    }

    pub fn all_tlcs(&self) -> impl Iterator<Item = &AddTlcInfo> {
        Self::filter_add_tlcs(
            self.local_pending_tlcs
                .tlcs()
                .into_iter()
                .chain(self.remote_pending_tlcs.tlcs().into_iter()),
        )
    }

    pub fn mark_tlc_remove(
        &mut self,
        tlc_id: TLCId,
        removed_at: CommitmentNumbers,
        reason: RemoveTlcReason,
    ) {
        // we don't consider RemoveTLC in the pending TLCS when build commitment signature
        // so it's safe to remove them all from remote and local pending TLCS
        self.local_pending_tlcs.drop_remove_tlc(&tlc_id);
        self.remote_pending_tlcs.drop_remove_tlc(&tlc_id);
        if let Some(tlc) = self.local_pending_tlcs.get_mut(&tlc_id) {
            tlc.removed_at = Some((removed_at, reason.clone()));
        }
        if let Some(tlc) = self.remote_pending_tlcs.get_mut(&tlc_id) {
            tlc.removed_at = Some((removed_at, reason));
        }
    }

    pub fn apply_remove_tlc(
        &mut self,
        tlc_id: TLCId,
        removed_at: CommitmentNumbers,
        reason: RemoveTlcReason,
    ) {
        self.mark_tlc_remove(tlc_id, removed_at, reason);
        // it's safe to remove multiple removed tlcs from pending TLCS,
        // just make sure the two partners are operating on correct pending list,
        // in other words, when one is remove from local TLCS,
        // the peer should remove it from remote TLCS
        if tlc_id.is_offered() {
            self.local_pending_tlcs.shrink_removed_tlc();
        } else {
            self.remote_pending_tlcs.shrink_removed_tlc();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct ChannelConstraints {
    // The maximum value can be in pending
    pub max_tlc_value_in_flight: u128,
    // The maximum number of tlcs that we can accept.
    pub max_tlc_number_in_flight: u64,
}

impl ChannelConstraints {
    pub fn new(max_tlc_value_in_flight: u128, max_tlc_number_in_flight: u64) -> Self {
        Self {
            max_tlc_value_in_flight,
            max_tlc_number_in_flight,
        }
    }

    pub fn default() -> Self {
        Self::new(
            DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
            DEFAULT_MAX_TLC_NUMBER_IN_FLIGHT,
        )
    }
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RevocationData {
    pub commitment_number: u64,
    pub x_only_aggregated_pubkey: [u8; 32],
    #[serde_as(as = "CompactSignatureAsBytes")]
    pub aggregated_signature: CompactSignature,
    #[serde_as(as = "EntityHex")]
    pub output: CellOutput,
    #[serde_as(as = "EntityHex")]
    pub output_data: Bytes,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SettlementData {
    pub x_only_aggregated_pubkey: [u8; 32],
    #[serde_as(as = "CompactSignatureAsBytes")]
    pub aggregated_signature: CompactSignature,
    #[serde_as(as = "EntityHex")]
    pub to_local_output: CellOutput,
    #[serde_as(as = "EntityHex")]
    pub to_local_output_data: Bytes,
    #[serde_as(as = "EntityHex")]
    pub to_remote_output: CellOutput,
    #[serde_as(as = "EntityHex")]
    pub to_remote_output_data: Bytes,
}

#[serde_as]
#[derive(Clone, Serialize, Deserialize)]
pub struct ChannelActorState {
    pub state: ChannelState,
    // The data below are only relevant if the channel is public.
    pub public_channel_info: Option<PublicChannelInfo>,

    // The local public key used to establish p2p network connection.
    pub local_pubkey: Pubkey,
    // The remote public key used to establish p2p network connection.
    pub remote_pubkey: Pubkey,

    pub id: Hash256,
    #[serde_as(as = "Option<EntityHex>")]
    pub funding_tx: Option<Transaction>,

    pub funding_tx_confirmed_at: Option<(BlockNumber, u32)>,

    #[serde_as(as = "Option<EntityHex>")]
    pub funding_udt_type_script: Option<Script>,

    // Is this channel initially inbound?
    // An inbound channel is one where the counterparty is the funder of the channel.
    pub is_acceptor: bool,

    // TODO: consider transaction fee while building the commitment transaction.
    // The invariant here is that the sum of `to_local_amount` and `to_remote_amount`
    // should be equal to the total amount of the channel.
    // The changes of both `to_local_amount` and `to_remote_amount`
    // will always happen after a revoke_and_ack message is sent/received.
    // This means that while calculating the amounts for commitment transactions,
    // processing add_tlc command and messages, we need to take into account that
    // the amounts are not decremented/incremented yet.

    // The amount of CKB/UDT that we own in the channel.
    // This value will only change after we have resolved a tlc.
    pub to_local_amount: u128,
    // The amount of CKB/UDT that the remote owns in the channel.
    // This value will only change after we have resolved a tlc.
    pub to_remote_amount: u128,

    // these two amounts used to keep the minimal ckb amount for the two parties
    // TLC operations will not affect these two amounts, only used to keep the commitment transactions
    // to be valid, so that any party can close the channel at any time.
    // Note: the values are different for the UDT scenario
    pub local_reserved_ckb_amount: u64,
    pub remote_reserved_ckb_amount: u64,

    // The commitment fee rate is used to calculate the fee for the commitment transactions.
    // The side who want to submit the commitment transaction will pay fee
    pub commitment_fee_rate: u64,

    // The delay time for the commitment transaction, this value is set by the initiator of the channel.
    // It must be a relative EpochNumberWithFraction in u64 format.
    pub commitment_delay_epoch: u64,

    // The fee rate used for funding transaction, the initiator may set it as `funding_fee_rate` option,
    // if it's not set, DEFAULT_FEE_RATE will be used as default value, two sides will use the same fee rate
    pub funding_fee_rate: u64,

    // Signer is used to sign the commitment transactions.
    pub signer: InMemorySigner,

    // Cached channel public keys for easier of access.
    pub local_channel_public_keys: ChannelBasePublicKeys,

    // Commitment numbers that are used to derive keys.
    // This value is guaranteed to be 0 when channel is just created.
    pub commitment_numbers: CommitmentNumbers,

    pub local_constraints: ChannelConstraints,
    pub remote_constraints: ChannelConstraints,

    // Below are fields that are only usable after the channel is funded,
    // (or at some point of the state).

    // all the TLC related information
    pub tlc_state: TlcState,

    // The remote and local lock script for close channel, they are setup during the channel establishment.
    #[serde_as(as = "Option<EntityHex>")]
    pub remote_shutdown_script: Option<Script>,
    #[serde_as(as = "EntityHex")]
    pub local_shutdown_script: Script,

    // While building a CommitmentSigned message, we use a nonce sent by the counterparty
    // to partially sign the commitment transaction. This nonce is also used while handling the revoke_and_ack
    // message from the peer. We need to save this nonce because the counterparty may send other nonces during
    // the period when our CommitmentSigned is sent and the counterparty's RevokeAndAck is received.
    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub last_used_nonce_in_commitment_signed: Option<PubNonce>,

    // The nonces that are sent by the counterparty, the length is at most 2
    #[serde_as(as = "Vec<(U64Hex, PubNonceAsBytes)>")]
    pub remote_nonces: Vec<(u64, PubNonce)>,

    // The latest commitment transaction we're holding,
    // it can be broadcasted to blockchain by us to force close the channel.
    #[serde_as(as = "Option<EntityHex>")]
    pub latest_commitment_transaction: Option<Transaction>,

    // All the commitment point that are sent from the counterparty.
    // We need to save all these points to derive the keys for the commitment transactions.
    // The length of this vector is at most the maximum number of flighting tlcs.
    pub remote_commitment_points: Vec<(u64, Pubkey)>,
    pub remote_channel_public_keys: Option<ChannelBasePublicKeys>,

    // The shutdown info for both local and remote, they are setup by the shutdown command or message.
    pub local_shutdown_info: Option<ShutdownInfo>,
    pub remote_shutdown_info: Option<ShutdownInfo>,

    // A flag to indicate whether the channel is reestablishing, we won't process any messages until the channel is reestablished.
    pub reestablishing: bool,

    pub created_at: SystemTime,
}

#[serde_as]
#[derive(Clone, Serialize, Deserialize, Eq, PartialEq, Debug)]
pub struct ShutdownInfo {
    #[serde_as(as = "EntityHex")]
    pub close_script: Script,
    pub fee_rate: u64,
    pub signature: Option<PartialSignature>,
}

// This struct holds the channel information that are only relevant when the channel
// is public. The information includes signatures to the channel announcement message,
// our config for the channel that will be published to the network (via ChannelUpdate).
// For ChannelUpdate config, only information on our side are saved here because we have no
// control to the config on the counterparty side. And they will publish
// the config to the network via another ChannelUpdate message.
#[serde_as]
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct PublicChannelInfo {
    pub enabled: bool,
    // The fee rate for tlc transfers. We only have these values set when
    // this is a public channel. Both sides may set this value differently.
    // This is a fee that is paid by the sender of the tlc.
    // The detailed calculation for the fee of forwarding tlcs is
    // `fee = round_above(tlc_fee_proportional_millionths * tlc_value / 1,000,000)`.
    pub tlc_fee_proportional_millionths: u128,

    // The expiry delta timestamp, in milliseconds, for the tlc.
    pub tlc_expiry_delta: u64,

    /// The minimal tcl value we can receive in relay tlc
    pub tlc_min_value: u128,

    // Channel announcement signatures, may be empty for private channel.
    pub local_channel_announcement_signature: Option<(EcdsaSignature, PartialSignature)>,
    pub remote_channel_announcement_signature: Option<(EcdsaSignature, PartialSignature)>,

    #[serde_as(as = "Option<PubNonceAsBytes>")]
    pub remote_channel_announcement_nonce: Option<PubNonce>,

    pub channel_announcement: Option<ChannelAnnouncement>,
    pub channel_update: Option<ChannelUpdate>,
}

impl PublicChannelInfo {
    pub fn new(
        tlc_min_value: u128,
        tlc_expiry_delta: u64,
        tlc_fee_proportional_millionths: u128,
    ) -> Self {
        Self {
            tlc_min_value,
            tlc_expiry_delta,
            tlc_fee_proportional_millionths,
            enabled: true,
            ..Default::default()
        }
    }
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct ClosedChannel {}

#[derive(Debug)]
pub enum ChannelEvent {
    PeerDisconnected,
    FundingTransactionConfirmed(BlockNumber, u32),
    CommitmentTransactionConfirmed,
    ClosingTransactionConfirmed,
    CheckTlcSetdown,
}

pub type ProcessingChannelResult = Result<(), ProcessingChannelError>;

#[derive(Error, Debug)]
pub enum ProcessingChannelError {
    #[error("Invalid state: {0}")]
    InvalidState(String),
    #[error("Repeated processing message: {0}")]
    RepeatedProcessing(String),
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("Capacity error: {0}")]
    CapacityError(#[from] CapacityError),
    #[error("Failed to spawn actor: {0}")]
    SpawnErr(#[from] SpawnErr),
    #[error("Musig2 VerifyError: {0}")]
    Musig2VerifyError(#[from] VerifyError),
    #[error("Musig2 SigningError: {0}")]
    Musig2SigningError(#[from] SigningError),
    #[error("Unable to handle TLC command in waiting TLC ACK state")]
    WaitingTlcAck,
    #[error("Failed to peel onion packet: {0}")]
    PeelingOnionPacketError(String),
    #[error("Forwarding node has tampered with the intended HTLC values or origin node has an obsolete cltv_expiry_delta")]
    IncorrectTlcExpiry,
    #[error("Upstream node set CLTV to less than the CLTV set by the sender")]
    IncorrectFinalTlcExpiry,
    #[error("The amount in the HTLC is not expected")]
    FinalIncorrectHTLCAmount,
    #[error("The payment_hash is not expected for final hop")]
    FinalIncorrectPaymentHash,
    #[error("The payment_hash and preimage does not match for final hop")]
    FinalIncorrectPreimage,
    #[error("The tlc forward fee is tow low")]
    TlcForwardFeeIsTooLow,
    #[error("The invoice status is invalid")]
    FinalInvoiceInvalid(CkbInvoiceStatus),
    #[error("The tlc number exceed limit of this channel")]
    TlcNumberExceedLimit,
    #[error("The tlc flight value exceed limit of this channel")]
    TlcValueInflightExceedLimit,
    #[error("The tlc amount below minimal")]
    TlcAmountIsTooLow,
    #[error("The tlc amount exceed maximal")]
    TlcAmountExceedLimit,
    #[error("The tlc expiry soon")]
    TlcExpirySoon,
    #[error("The tlc expiry too far")]
    TlcExpiryTooFar,
    #[error("Tlc forwarding error")]
    TlcForwardingError(TlcErr),
}

/// ProcessingChannelError which brings the shared secret used in forwarding onion packet.
/// The shared secret is required to obfuscate the error message.
#[derive(Error, Debug)]
#[error("{source}")]
pub struct ProcessingChannelErrorWithSharedSecret {
    pub source: ProcessingChannelError,
    /// Shared secret used in forwarding.
    ///
    /// Save it to backward errors. Use all zeros when no shared secrets are available.
    pub shared_secret: [u8; 32],
}

impl ProcessingChannelError {
    pub fn with_shared_secret(
        self,
        shared_secret: [u8; 32],
    ) -> ProcessingChannelErrorWithSharedSecret {
        ProcessingChannelErrorWithSharedSecret {
            source: self,
            shared_secret,
        }
    }

    pub fn without_shared_secret(self) -> ProcessingChannelErrorWithSharedSecret {
        self.with_shared_secret(NO_SHARED_SECRET.clone())
    }
}

bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ChannelFlags: u8 {
        const PUBLIC = 1;
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct NegotiatingFundingFlags: u32 {
        const OUR_INIT_SENT = 1;
        const THEIR_INIT_SENT = 1 << 1;
        const INIT_SENT = NegotiatingFundingFlags::OUR_INIT_SENT.bits() | NegotiatingFundingFlags::THEIR_INIT_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct CollaboratingFundingTxFlags: u32 {
        const AWAITING_REMOTE_TX_COLLABORATION_MSG = 1;
        const PREPARING_LOCAL_TX_COLLABORATION_MSG = 1 << 1;
        const OUR_TX_COMPLETE_SENT = 1 << 2;
        const THEIR_TX_COMPLETE_SENT = 1 << 3;
        const COLLABRATION_COMPLETED = CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT.bits() | CollaboratingFundingTxFlags::THEIR_TX_COMPLETE_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct SigningCommitmentFlags: u32 {
        const OUR_COMMITMENT_SIGNED_SENT = 1;
        const THEIR_COMMITMENT_SIGNED_SENT = 1 << 1;
        const COMMITMENT_SIGNED_SENT = SigningCommitmentFlags::OUR_COMMITMENT_SIGNED_SENT.bits() | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct AwaitingTxSignaturesFlags: u32 {
        const OUR_TX_SIGNATURES_SENT = 1;
        const THEIR_TX_SIGNATURES_SENT = 1 << 1;
        const TX_SIGNATURES_SENT = AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT.bits() | AwaitingTxSignaturesFlags::THEIR_TX_SIGNATURES_SENT.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct AwaitingChannelReadyFlags: u32 {
        const OUR_CHANNEL_READY = 1;
        const THEIR_CHANNEL_READY = 1 << 1;
        const CHANNEL_READY = AwaitingChannelReadyFlags::OUR_CHANNEL_READY.bits() | AwaitingChannelReadyFlags::THEIR_CHANNEL_READY.bits();
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ShuttingDownFlags: u32 {
        /// Indicates that we have sent a `shutdown` message.
        const OUR_SHUTDOWN_SENT = 1;
        /// Indicates that they have sent a `shutdown` message.
        const THEIR_SHUTDOWN_SENT = 1 << 1;
        /// Indicates that both we and they have sent `shutdown` messages,
        /// but some HTLCs are still pending to be resolved.
        const AWAITING_PENDING_TLCS = ShuttingDownFlags::OUR_SHUTDOWN_SENT.bits() | ShuttingDownFlags::THEIR_SHUTDOWN_SENT.bits();
        /// Indicates all pending HTLCs are resolved, and this channel will be dropped.
        const DROPPING_PENDING = 1 << 2;
        /// Indicates we have submitted a commitment transaction, waiting for confirmation
        const WAITING_COMMITMENT_CONFIRMATION = 1 << 3;
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct CloseFlags: u32 {
        /// Indicates that channel is closed cooperatively.
        const COOPERATIVE = 1;
        /// Indicates that channel is closed uncooperatively, initiated by one party forcely.
        const UNCOOPERATIVE = 1 << 1;
    }
}

// Depending on the state of the channel, we may process the commitment_signed command differently.
// Below are all the channel state flags variants that we may encounter
// in normal commitment_signed processing flow.
#[derive(Debug)]
enum CommitmentSignedFlags {
    SigningCommitment(SigningCommitmentFlags),
    PendingShutdown(),
    ChannelReady(),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelState {
    /// We are negotiating the parameters required for the channel prior to funding it.
    NegotiatingFunding(NegotiatingFundingFlags),
    /// We're collaborating with the other party on the funding transaction.
    CollaboratingFundingTx(CollaboratingFundingTxFlags),
    /// We have collaborated over the funding and are now waiting for CommitmentSigned messages.
    SigningCommitment(SigningCommitmentFlags),
    /// We've received and sent `commitment_signed` and are now waiting for both
    /// party to collaborate on creating a valid funding transaction.
    AwaitingTxSignatures(AwaitingTxSignaturesFlags),
    /// We've received/sent `funding_created` and `funding_signed` and are thus now waiting on the
    /// funding transaction to confirm.
    AwaitingChannelReady(AwaitingChannelReadyFlags),
    /// Both we and our counterparty consider the funding transaction confirmed and the channel is
    /// now operational.
    ChannelReady(),
    /// We've successfully negotiated a `closing_signed` dance. At this point, the `ChannelManager`
    /// is about to drop us, but we store this anyway.
    ShuttingDown(ShuttingDownFlags),
    /// This channel is closed.
    Closed(CloseFlags),
}

impl ChannelState {
    fn is_closed(&self) -> bool {
        matches!(self, ChannelState::Closed(_))
    }
}

fn new_channel_id_from_seed(seed: &[u8]) -> Hash256 {
    blake2b_256(seed).into()
}

fn derive_channel_id_from_tlc_keys(tlc_basepoint1: &Pubkey, tlc_basepoint2: &Pubkey) -> Hash256 {
    let mut preimage = [tlc_basepoint1.0.serialize(), tlc_basepoint2.0.serialize()];
    preimage.sort();
    new_channel_id_from_seed(&preimage.concat())
}

fn derive_temp_channel_id_from_tlc_key(tlc_basepoint: &Pubkey) -> Hash256 {
    let preimage = [tlc_basepoint.0.serialize(), [0; 33]].concat();
    new_channel_id_from_seed(&preimage)
}

pub fn get_commitment_secret(commitment_seed: &[u8; 32], commitment_number: u64) -> [u8; 32] {
    // Note that here, we hold the same assumption to bolts for commitment number,
    // i.e. this number should be in the range [0, 2^48).
    let mut res: [u8; 32] = *commitment_seed;
    for i in 0..48 {
        let bitpos = 47 - i;
        if commitment_number & (1 << bitpos) == (1 << bitpos) {
            res[bitpos / 8] ^= 1 << (bitpos & 7);
            res = blake2b_256(res);
        }
    }
    res
}

pub fn get_commitment_point(commitment_seed: &[u8; 32], commitment_number: u64) -> Pubkey {
    Privkey::from(&get_commitment_secret(commitment_seed, commitment_number)).pubkey()
}

pub(crate) fn get_funding_and_reserved_amount(
    total_amount: u128,
    shutdown_script: &Script,
    udt_type_script: &Option<Script>,
) -> Result<(u128, u64), ProcessingChannelError> {
    let reserved_capacity = reserved_capacity(shutdown_script, udt_type_script)?.as_u64();
    if udt_type_script.is_none() {
        if total_amount < reserved_capacity as u128 {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "The funding amount ({}) should be greater than or equal to {}",
                total_amount, reserved_capacity
            )));
        }
        if total_amount >= u64::MAX as u128 {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "The funding amount ({}) should be less than {}",
                total_amount,
                u64::MAX
            )));
        }
        Ok((total_amount - reserved_capacity as u128, reserved_capacity))
    } else {
        Ok((total_amount, reserved_capacity))
    }
}

pub(crate) fn reserved_capacity(
    shutdown_script: &Script,
    udt_type_script: &Option<Script>,
) -> Result<Capacity, CapacityError> {
    occupied_capacity(shutdown_script, udt_type_script)?
        .safe_add(Capacity::shannons(DEFAULT_MIN_SHUTDOWN_FEE))
}

pub(crate) fn occupied_capacity(
    shutdown_script: &Script,
    udt_type_script: &Option<Script>,
) -> Result<Capacity, CapacityError> {
    let cell_output = CellOutput::new_builder()
        .lock(shutdown_script.clone())
        .type_(udt_type_script.clone().pack())
        .build();

    if udt_type_script.is_some() {
        // 16 bytes for udt data
        cell_output.occupied_capacity(Capacity::bytes(16)?)
    } else {
        cell_output.occupied_capacity(Capacity::bytes(0)?)
    }
}

impl From<&ChannelActorState> for Musig2SignContext {
    fn from(value: &ChannelActorState) -> Self {
        Musig2SignContext {
            key_agg_ctx: value.get_musig2_agg_context(),
            agg_nonce: value.get_musig2_agg_pubnonce(),
            seckey: value.signer.funding_key.clone(),
            secnonce: value.get_local_musig2_secnonce(),
        }
    }
}

impl From<&ChannelActorState> for Musig2VerifyContext {
    fn from(value: &ChannelActorState) -> Self {
        Musig2VerifyContext {
            key_agg_ctx: value.get_musig2_agg_context(),
            agg_nonce: value.get_musig2_agg_pubnonce(),
            pubkey: *value.get_remote_funding_pubkey(),
            pubnonce: value.get_remote_nonce().clone(),
        }
    }
}

impl From<(&ChannelActorState, bool)> for Musig2SignContext {
    fn from(value: (&ChannelActorState, bool)) -> Self {
        let (channel, local) = value;
        let local_pubkey = channel.get_local_channel_public_keys().funding_pubkey;
        let remote_pubkey = channel.get_remote_channel_public_keys().funding_pubkey;
        let pubkeys = if local {
            [local_pubkey, remote_pubkey]
        } else {
            [remote_pubkey, local_pubkey]
        };
        let key_agg_ctx = KeyAggContext::new(pubkeys).expect("Valid pubkeys");

        let local_nonce = channel.get_local_nonce();
        let remote_nonce = channel.get_remote_nonce();
        let nonces = if local {
            [local_nonce, remote_nonce]
        } else {
            [remote_nonce, local_nonce]
        };
        let agg_nonce = AggNonce::sum(nonces);

        Musig2SignContext {
            key_agg_ctx,
            agg_nonce,
            seckey: channel.signer.funding_key.clone(),
            secnonce: channel.get_local_musig2_secnonce(),
        }
    }
}

impl From<(&ChannelActorState, bool)> for Musig2VerifyContext {
    fn from(value: (&ChannelActorState, bool)) -> Self {
        let (channel, local) = value;
        let local_pubkey = channel.get_local_channel_public_keys().funding_pubkey;
        let remote_pubkey = channel.get_remote_channel_public_keys().funding_pubkey;
        let pubkeys = if local {
            [local_pubkey, remote_pubkey]
        } else {
            [remote_pubkey, local_pubkey]
        };
        let key_agg_ctx = KeyAggContext::new(pubkeys).expect("Valid pubkeys");

        let local_nonce = channel.get_local_nonce();
        let remote_nonce = channel.get_remote_nonce();
        let nonces = if local {
            [local_nonce, remote_nonce]
        } else {
            [remote_nonce, local_nonce]
        };
        let agg_nonce = AggNonce::sum(nonces);

        Musig2VerifyContext {
            key_agg_ctx,
            agg_nonce,
            pubkey: *channel.get_remote_funding_pubkey(),
            pubnonce: channel.get_remote_nonce(),
        }
    }
}

// Constructors for the channel actor state.
#[allow(clippy::too_many_arguments)]
impl ChannelActorState {
    pub fn is_public(&self) -> bool {
        self.public_channel_info.is_some()
    }

    pub async fn try_create_channel_messages(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
    ) -> Option<(ChannelAnnouncement, ChannelUpdate)> {
        let channel_announcement = self
            .try_create_channel_announcement_message(network)
            .await?;
        let channel_update = self.try_create_channel_update_message(network).await?;
        Some((channel_announcement, channel_update))
    }

    pub async fn try_create_channel_announcement_message(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
    ) -> Option<ChannelAnnouncement> {
        if !self.is_public() {
            debug!("Ignoring non-public channel announcement");
            return None;
        }

        let mut channel_announcement = match self
            .public_channel_info
            .as_ref()
            .and_then(|state| state.channel_announcement.clone())
        {
            // Skipping creating new signed channel announcement if it exists
            Some(x) if x.is_signed() => return Some(x),
            // We have created a channel announcement, but it's not signed by the other
            // party yet. We should try to complete the signatures next.
            Some(x) => x,
            // We have not created a channel announcement yet.
            None => {
                let channel_outpoint = self.must_get_funding_transaction_outpoint();
                let capacity = self.get_liquid_capacity();
                let (node1_id, node2_id) = if self.local_is_node1() {
                    (self.local_pubkey, self.remote_pubkey)
                } else {
                    (self.remote_pubkey, self.local_pubkey)
                };
                let channel_announcement = ChannelAnnouncement::new_unsigned(
                    &node1_id,
                    &node2_id,
                    channel_outpoint,
                    &self.get_funding_lock_script_xonly_key(),
                    capacity,
                    self.funding_udt_type_script.clone(),
                );
                channel_announcement
            }
        };

        let local_nonce = self
            .get_channel_announcement_musig2_secnonce()
            .public_nonce();
        let remote_nonce = self.get_remote_channel_announcement_nonce()?;
        let agg_nonce =
            AggNonce::sum(self.order_things_for_musig2(local_nonce, remote_nonce.clone()));

        let key_agg_ctx = self.get_musig2_agg_context();

        let message = channel_announcement.message_to_sign();

        let (local_node_signature, local_partial_signature) = self
            .get_or_create_local_channel_announcement_signature(
                remote_nonce.clone(),
                message,
                network,
            )
            .await;

        let (remote_node_signature, remote_partial_signature) =
            self.get_remote_channel_announcement_signature()?;

        if self.local_is_node1() {
            channel_announcement.node1_signature = Some(local_node_signature);
            channel_announcement.node2_signature = Some(remote_node_signature);
        } else {
            channel_announcement.node1_signature = Some(remote_node_signature);
            channel_announcement.node2_signature = Some(local_node_signature);
        }

        let partial_signatures =
            self.order_things_for_musig2(local_partial_signature, remote_partial_signature);

        let signature =
            aggregate_partial_signatures(&key_agg_ctx, &agg_nonce, partial_signatures, message)
                .expect("aggregate partial signatures");

        channel_announcement.ckb_signature = Some(signature);

        self.public_channel_state_mut().channel_announcement = Some(channel_announcement.clone());

        Some(channel_announcement)
    }

    async fn do_generate_channel_update(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
        // The function that would change the channel update parameters.
        f: impl FnOnce(&mut ChannelUpdate),
    ) -> ChannelUpdate {
        let mut channel_update = self
            .get_unsigned_channel_update_message()
            .expect("public channel can generate channel update message");
        f(&mut channel_update);
        debug!(
            "Generated channel update message for channel {:?}: {:?}",
            &self.get_id(),
            &channel_update
        );
        let node_signature =
            sign_network_message(network.clone(), channel_update.message_to_sign())
                .await
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        channel_update.signature = Some(node_signature);
        self.public_channel_state_mut().channel_update = Some(channel_update.clone());
        channel_update
    }

    async fn generate_channel_update(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ChannelUpdate {
        self.do_generate_channel_update(network, |_update| {}).await
    }

    async fn generate_disabled_channel_update(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ChannelUpdate {
        self.do_generate_channel_update(network, |update| {
            update.channel_flags = CHANNEL_DISABLED_FLAG
        })
        .await
    }

    async fn generate_and_broadcast_channel_update(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
    ) {
        let channel_update = self.generate_channel_update(network).await;
        network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::BroadcastMessages(vec![BroadcastMessage::ChannelUpdate(
                    channel_update,
                )]),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }

    async fn try_create_channel_update_message(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
    ) -> Option<ChannelUpdate> {
        if !self.is_public() {
            debug!("Ignoring non-public channel update");
            return None;
        }

        match self
            .public_channel_info
            .as_ref()
            .and_then(|state| state.channel_update.clone())
        {
            Some(x) => return Some(x),
            _ => {}
        };

        Some(self.generate_channel_update(network).await)
    }

    pub fn get_unsigned_channel_update_message(&self) -> Option<ChannelUpdate> {
        let local_is_node1 = self.local_is_node1();
        let message_flags = if local_is_node1 { 0 } else { 1 };

        self.public_channel_info.as_ref().and_then(|info| {
            Some(ChannelUpdate::new_unsigned(
                self.must_get_funding_transaction_outpoint(),
                now_timestamp_as_millis_u64(),
                message_flags,
                0,
                info.tlc_expiry_delta,
                info.tlc_min_value,
                info.tlc_fee_proportional_millionths,
            ))
        })
    }

    pub fn new_inbound_channel<'a>(
        temp_channel_id: Hash256,
        public_channel_info: Option<PublicChannelInfo>,
        local_value: u128,
        local_reserved_ckb_amount: u64,
        commitment_fee_rate: u64,
        commitment_delay_epoch: u64,
        funding_fee_rate: u64,
        funding_udt_type_script: Option<Script>,
        seed: &[u8],
        local_pubkey: Pubkey,
        remote_pubkey: Pubkey,
        local_shutdown_script: Script,
        remote_shutdown_script: Script,
        remote_value: u128,
        remote_reserved_ckb_amount: u64,
        remote_pubkeys: ChannelBasePublicKeys,
        remote_nonce: PubNonce,
        remote_channel_announcement_nonce: Option<PubNonce>,
        first_commitment_point: Pubkey,
        second_commitment_point: Pubkey,
        remote_max_tlc_value_in_flight: u128,
        remote_max_tlc_number_in_flight: u64,
        local_max_tlc_number_in_flight: u64,
        local_max_tlc_value_in_flight: u128,
    ) -> Self {
        let signer = InMemorySigner::generate_from_seed(seed);
        let local_base_pubkeys = signer.get_base_public_keys();

        let channel_id = derive_channel_id_from_tlc_keys(
            &local_base_pubkeys.tlc_base_key,
            &remote_pubkeys.tlc_base_key,
        );

        debug!(
            "Generated channel id ({:?}) for temporary channel {:?}",
            &channel_id, &temp_channel_id,
        );

        let mut state = Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::THEIR_INIT_SENT),
            public_channel_info,
            local_pubkey,
            remote_pubkey,
            funding_tx: None,
            funding_tx_confirmed_at: None,
            is_acceptor: true,
            funding_udt_type_script,
            to_local_amount: local_value,
            to_remote_amount: remote_value,
            commitment_fee_rate,
            commitment_delay_epoch,
            funding_fee_rate,
            id: channel_id,
            tlc_state: Default::default(),
            local_shutdown_script: local_shutdown_script,
            local_channel_public_keys: local_base_pubkeys,
            signer,
            remote_channel_public_keys: Some(remote_pubkeys),
            commitment_numbers: Default::default(),
            remote_shutdown_script: Some(remote_shutdown_script),
            last_used_nonce_in_commitment_signed: None,
            remote_nonces: vec![(0, remote_nonce)],
            remote_commitment_points: vec![
                (0, first_commitment_point),
                (1, second_commitment_point),
            ],
            local_shutdown_info: None,
            remote_shutdown_info: None,
            local_reserved_ckb_amount,
            remote_reserved_ckb_amount,
            local_constraints: ChannelConstraints::new(
                local_max_tlc_value_in_flight,
                local_max_tlc_number_in_flight,
            ),
            remote_constraints: ChannelConstraints::new(
                remote_max_tlc_value_in_flight,
                remote_max_tlc_number_in_flight,
            ),
            latest_commitment_transaction: None,
            reestablishing: false,
            created_at: SystemTime::now(),
        };
        if let Some(nonce) = remote_channel_announcement_nonce {
            state.update_remote_channel_announcement_nonce(&nonce);
        }
        state
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_outbound_channel(
        public_channel_info: Option<PublicChannelInfo>,
        seed: &[u8],
        local_pubkey: Pubkey,
        remote_pubkey: Pubkey,
        to_local_amount: u128,
        local_reserved_ckb_amount: u64,
        commitment_fee_rate: u64,
        commitment_delay_epoch: u64,
        funding_fee_rate: u64,
        funding_udt_type_script: Option<Script>,
        shutdown_script: Script,
        local_max_tlc_value_in_flight: u128,
        local_max_tlc_number_in_flight: u64,
    ) -> Self {
        let signer = InMemorySigner::generate_from_seed(seed);
        let local_pubkeys = signer.get_base_public_keys();
        let temp_channel_id = derive_temp_channel_id_from_tlc_key(&local_pubkeys.tlc_base_key);
        Self {
            state: ChannelState::NegotiatingFunding(NegotiatingFundingFlags::empty()),
            public_channel_info,
            local_pubkey,
            remote_pubkey,
            funding_tx: None,
            funding_tx_confirmed_at: None,
            funding_udt_type_script,
            is_acceptor: false,
            to_local_amount,
            to_remote_amount: 0,
            commitment_fee_rate,
            commitment_delay_epoch,
            funding_fee_rate,
            id: temp_channel_id,
            tlc_state: Default::default(),
            signer,
            local_channel_public_keys: local_pubkeys,
            local_constraints: ChannelConstraints::new(
                local_max_tlc_value_in_flight,
                local_max_tlc_number_in_flight,
            ),
            // these values will update after accept channel peer message handled
            remote_constraints: ChannelConstraints::default(),
            remote_channel_public_keys: None,
            last_used_nonce_in_commitment_signed: None,
            remote_nonces: vec![],
            commitment_numbers: Default::default(),
            remote_commitment_points: vec![],
            local_shutdown_script: shutdown_script,
            remote_shutdown_script: None,
            local_shutdown_info: None,
            remote_shutdown_info: None,
            local_reserved_ckb_amount,
            remote_reserved_ckb_amount: 0,
            latest_commitment_transaction: None,
            reestablishing: false,
            created_at: SystemTime::now(),
        }
    }

    // TODO: this fn is duplicated with NetworkActorState::check_open_channel_parameters, but is not easy to refactor, just keep it for now.
    fn check_open_channel_parameters(&self) -> ProcessingChannelResult {
        let udt_type_script = &self.funding_udt_type_script;

        // reserved_ckb_amount
        let occupied_capacity =
            occupied_capacity(&self.local_shutdown_script, udt_type_script)?.as_u64();
        if self.local_reserved_ckb_amount < occupied_capacity {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Reserved CKB amount {} is less than {}",
                self.local_reserved_ckb_amount, occupied_capacity,
            )));
        }

        // funding_fee_rate
        if self.funding_fee_rate < DEFAULT_FEE_RATE {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Funding fee rate is less than {}",
                DEFAULT_FEE_RATE,
            )));
        }

        // commitment_fee_rate
        if self.commitment_fee_rate < DEFAULT_COMMITMENT_FEE_RATE {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment fee rate is less than {}",
                DEFAULT_COMMITMENT_FEE_RATE,
            )));
        }
        let commitment_fee = calculate_commitment_tx_fee(self.commitment_fee_rate, udt_type_script);
        let reserved_fee = self.local_reserved_ckb_amount - occupied_capacity;
        if commitment_fee * 2 > reserved_fee {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment fee {} which caculated by commitment fee rate {} is larger than half of reserved fee {}",
                commitment_fee, self.commitment_fee_rate, reserved_fee
            )));
        }

        // commitment_delay_epoch
        let epoch = EpochNumberWithFraction::from_full_value_unchecked(self.commitment_delay_epoch);
        if !epoch.is_well_formed() {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment delay epoch {} is not a valid value",
                self.commitment_delay_epoch,
            )));
        }

        let min = EpochNumberWithFraction::new(MIN_COMMITMENT_DELAY_EPOCHS, 0, 1);
        if epoch < min {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment delay epoch {} is less than the minimal value {}",
                epoch, min
            )));
        }

        let max = EpochNumberWithFraction::new(MAX_COMMITMENT_DELAY_EPOCHS, 0, 1);
        if epoch > max {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment delay epoch {} is greater than the maximal value {}",
                epoch, max
            )));
        }

        // max_tlc_number_in_flight
        if self.local_constraints.max_tlc_number_in_flight > SYS_MAX_TLC_NUMBER_IN_FLIGHT {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Local max TLC number in flight {} is greater than the system maximal value {}",
                self.local_constraints.max_tlc_number_in_flight, SYS_MAX_TLC_NUMBER_IN_FLIGHT
            )));
        }

        Ok(())
    }

    fn check_accept_channel_parameters(&self) -> Result<(), ProcessingChannelError> {
        if self.remote_constraints.max_tlc_number_in_flight > MAX_TLC_NUMBER_IN_FLIGHT {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Remote max TLC number in flight {} is greater than the system maximal value {}",
                self.remote_constraints.max_tlc_number_in_flight, MAX_TLC_NUMBER_IN_FLIGHT
            )));
        }

        let udt_type_script = &self.funding_udt_type_script;

        // reserved_ckb_amount
        let occupied_capacity =
            occupied_capacity(&self.get_remote_shutdown_script(), udt_type_script)?.as_u64();
        if self.remote_reserved_ckb_amount < occupied_capacity {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Reserved CKB amount {} is less than {}",
                self.remote_reserved_ckb_amount, occupied_capacity,
            )));
        }

        // commitment_fee_rate
        let commitment_fee = calculate_commitment_tx_fee(self.commitment_fee_rate, udt_type_script);
        let reserved_fee = self.remote_reserved_ckb_amount - occupied_capacity;
        if commitment_fee * 2 > reserved_fee {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Commitment fee {} which caculated by commitment fee rate {} is larger than half of reserved fee {}",
                commitment_fee, self.commitment_fee_rate, reserved_fee
            )));
        }

        Ok(())
    }

    fn check_shutdown_fee_rate(
        &self,
        fee_rate: FeeRate,
        close_script: &Script,
    ) -> ProcessingChannelResult {
        if fee_rate.as_u64() < self.commitment_fee_rate {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Fee rate {} is less than commitment fee rate {}",
                fee_rate, self.commitment_fee_rate
            )));
        }

        let fee = calculate_shutdown_tx_fee(
            fee_rate.as_u64(),
            &self.funding_udt_type_script,
            (self.get_remote_shutdown_script(), close_script.clone()),
        );

        let occupied_capacity =
            occupied_capacity(close_script, &self.funding_udt_type_script)?.as_u64();
        let available_max_fee = if self.funding_udt_type_script.is_none() {
            (self.to_local_amount as u64 + self.local_reserved_ckb_amount)
                .saturating_sub(occupied_capacity)
        } else {
            self.local_reserved_ckb_amount
                .saturating_sub(occupied_capacity)
        };

        if fee > available_max_fee {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Local balance is not enough to pay the fee, expect fee {} <= available_max_fee {}",
                fee, available_max_fee
            )));
        }
        Ok(())
    }

    pub fn get_local_balance(&self) -> u128 {
        self.to_local_amount
    }

    pub fn get_remote_balance(&self) -> u128 {
        self.to_remote_amount
    }

    pub fn get_offered_tlc_balance(&self) -> u128 {
        self.get_all_offer_tlcs()
            .map(|tlc| tlc.amount)
            .sum::<u128>()
    }

    pub fn get_received_tlc_balance(&self) -> u128 {
        self.get_all_received_tlcs()
            .map(|tlc| tlc.amount)
            .sum::<u128>()
    }

    pub fn get_created_at_in_millis(&self) -> u64 {
        self.created_at
            .duration_since(UNIX_EPOCH)
            .expect("Duration since unix epoch")
            .as_millis() as u64
    }

    pub fn is_closed(&self) -> bool {
        self.state.is_closed()
    }

    pub(crate) fn update_state(&mut self, new_state: ChannelState) {
        debug!(
            "Updating channel state from {:?} to {:?}",
            &self.state, &new_state
        );
        self.state = new_state;
    }

    fn local_is_node1(&self) -> bool {
        self.local_pubkey < self.remote_pubkey
    }

    async fn get_or_create_local_channel_announcement_signature(
        &mut self,
        remote_nonce: PubNonce,
        message: [u8; 32],
        network: &ActorRef<NetworkActorMessage>,
    ) -> (EcdsaSignature, PartialSignature) {
        if let Some(local_channel_announcement_signature) = self
            .public_channel_info
            .as_ref()
            .and_then(|channel_info| channel_info.local_channel_announcement_signature.clone())
        {
            return local_channel_announcement_signature;
        }

        let local_secnonce = self.get_channel_announcement_musig2_secnonce();
        let local_nonce = local_secnonce.public_nonce();
        let agg_nonce = AggNonce::sum(self.order_things_for_musig2(local_nonce, remote_nonce));
        let key_agg_ctx = self.get_musig2_agg_context();
        let channel_id = self.get_id();
        let peer_id = self.get_remote_peer_id();
        let channel_outpoint = self.must_get_funding_transaction_outpoint();

        let partial_signature: PartialSignature = sign_partial(
            &key_agg_ctx,
            &self.signer.funding_key,
            local_secnonce,
            &agg_nonce,
            message,
        )
        .expect("Partial sign channel announcement");

        let node_signature = sign_network_message(network.clone(), message)
            .await
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                    peer_id,
                    FiberMessage::announcement_signatures(AnnouncementSignatures {
                        channel_id,
                        channel_outpoint,
                        partial_signature,
                        node_signature: node_signature.clone(),
                    }),
                )),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        let result = (node_signature, partial_signature);
        self.public_channel_state_mut()
            .local_channel_announcement_signature = Some(result.clone());
        result
    }

    fn public_channel_state_mut(&mut self) -> &mut PublicChannelInfo {
        self.public_channel_info
            .as_mut()
            .expect("public channel info exists")
    }

    fn get_remote_channel_announcement_nonce(&self) -> Option<PubNonce> {
        self.public_channel_info
            .as_ref()
            .and_then(|state| state.remote_channel_announcement_nonce.clone())
    }

    fn update_remote_channel_announcement_nonce(&mut self, nonce: &PubNonce) {
        assert!(self.is_public());
        self.public_channel_state_mut()
            .remote_channel_announcement_nonce = Some(nonce.clone());
    }

    fn get_remote_channel_announcement_signature(
        &self,
    ) -> Option<(EcdsaSignature, PartialSignature)> {
        self.public_channel_info
            .as_ref()
            .and_then(|state| state.remote_channel_announcement_signature.clone())
    }

    fn update_remote_channel_announcement_signature(
        &mut self,
        ecdsa_signature: EcdsaSignature,
        partial_signatures: PartialSignature,
    ) {
        assert!(self.is_public());
        self.public_channel_info
            .as_mut()
            .expect("public channel info exists")
            .remote_channel_announcement_signature = Some((ecdsa_signature, partial_signatures));
    }

    fn get_our_tlc_fee_proportional_millionths(&self) -> Option<u128> {
        self.public_channel_info
            .as_ref()
            .map(|state| state.tlc_fee_proportional_millionths)
    }

    fn update_our_tlc_fee_proportional_millionths(&mut self, fee: u128) -> bool {
        let old_fee = self.get_our_tlc_fee_proportional_millionths();
        match old_fee {
            Some(old_fee) if old_fee == fee => false,
            _ => {
                self.public_channel_state_mut()
                    .tlc_fee_proportional_millionths = fee;
                true
            }
        }
    }

    fn get_our_tlc_min_value(&self) -> Option<u128> {
        self.public_channel_info
            .as_ref()
            .map(|state| state.tlc_min_value)
    }

    fn update_our_tlc_min_value(&mut self, value: u128) -> bool {
        let old_value = self.get_our_tlc_min_value();
        match old_value {
            Some(old_value) if old_value == value => false,
            _ => {
                self.public_channel_state_mut().tlc_min_value = value;
                true
            }
        }
    }

    fn get_our_enabled(&self) -> Option<bool> {
        self.public_channel_info.as_ref().map(|state| state.enabled)
    }

    fn update_our_enabled(&mut self, enabled: bool) -> bool {
        let old_value = self.get_our_enabled();
        match old_value {
            Some(old_value) if old_value == enabled => false,
            _ => {
                self.public_channel_state_mut().enabled = enabled;
                true
            }
        }
    }

    fn get_our_tlc_expiry_delta(&self) -> Option<u64> {
        self.public_channel_info
            .as_ref()
            .map(|info| info.tlc_expiry_delta)
    }

    fn update_our_tlc_expiry_delta(&mut self, value: u64) -> bool {
        let old_value = self.get_our_tlc_expiry_delta();
        match old_value {
            Some(old_value) if old_value == value => false,
            _ => {
                self.public_channel_state_mut().tlc_expiry_delta = value;
                true
            }
        }
    }

    fn get_total_reserved_ckb_amount(&self) -> u64 {
        self.local_reserved_ckb_amount + self.remote_reserved_ckb_amount
    }

    fn get_total_ckb_amount(&self) -> u64 {
        self.to_local_amount as u64
            + self.to_remote_amount as u64
            + self.get_total_reserved_ckb_amount()
    }

    fn get_total_udt_amount(&self) -> u128 {
        self.to_local_amount + self.to_remote_amount
    }

    // Get the total liquid capacity of the channel, which will exclude the reserved ckb amount.
    // This is the capacity used for gossiping channel information.
    fn get_liquid_capacity(&self) -> u128 {
        let capacity = if self.funding_udt_type_script.is_some() {
            self.get_total_udt_amount()
        } else {
            self.to_local_amount as u128 + self.to_remote_amount as u128
        };
        capacity
    }

    // Send RevokeAndAck message to the counterparty, and update the
    // channel state accordingly.
    fn send_revoke_and_ack_message(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        let key_agg_ctx = {
            let local_pubkey = self.get_local_channel_public_keys().funding_pubkey;
            let remote_pubkey = self.get_remote_channel_public_keys().funding_pubkey;
            KeyAggContext::new([remote_pubkey, local_pubkey]).expect("Valid pubkeys")
        };
        let x_only_aggregated_pubkey = key_agg_ctx.aggregated_pubkey::<Point>().serialize_xonly();
        let sign_ctx = {
            let local_nonce = self.get_local_nonce();
            let remote_nonce = self.get_remote_nonce();
            let nonces = [local_nonce, remote_nonce];
            let agg_nonce = AggNonce::sum(nonces);
            Musig2SignContext {
                key_agg_ctx,
                agg_nonce,
                seckey: self.signer.funding_key.clone(),
                secnonce: self.get_local_musig2_secnonce(),
            }
        };

        let revocation_partial_signature = {
            let commitment_tx_fee = calculate_commitment_tx_fee(
                self.commitment_fee_rate,
                &self.funding_udt_type_script,
            );
            let lock_script = self.get_remote_shutdown_script();
            let (output, output_data) = if let Some(udt_type_script) = &self.funding_udt_type_script
            {
                let capacity = self.get_total_reserved_ckb_amount() - commitment_tx_fee;
                let output = CellOutput::new_builder()
                    .lock(lock_script)
                    .type_(Some(udt_type_script.clone()).pack())
                    .capacity(capacity.pack())
                    .build();

                let output_data = self.get_total_udt_amount().to_le_bytes().pack();
                (output, output_data)
            } else {
                let capacity = self.get_total_ckb_amount() - commitment_tx_fee;
                let output = CellOutput::new_builder()
                    .lock(lock_script)
                    .capacity(capacity.pack())
                    .build();
                let output_data = Bytes::default();
                (output, output_data)
            };

            let commitment_number = self.get_remote_commitment_number() - 1;
            let commitment_lock_script_args = [
                &blake2b_256(x_only_aggregated_pubkey)[0..20],
                self.get_delay_epoch_as_lock_args_bytes().as_slice(),
                commitment_number.to_be_bytes().as_slice(),
            ]
            .concat();

            let message = blake2b_256(
                [
                    output.as_slice(),
                    output_data.as_slice(),
                    commitment_lock_script_args.as_slice(),
                ]
                .concat(),
            );
            sign_ctx
                .clone()
                .sign(message.as_slice())
                .expect("valid signature")
        };

        let commitment_tx_partial_signature = {
            let (
                [to_local_output, to_remote_output],
                [to_local_output_data, to_remote_output_data],
            ) = self.build_settlement_transaction_outputs(false);
            let commitment_lock_script_args = [
                &blake2b_256(x_only_aggregated_pubkey)[0..20],
                self.get_delay_epoch_as_lock_args_bytes().as_slice(),
                self.get_remote_commitment_number().to_be_bytes().as_slice(),
            ]
            .concat();
            let message = blake2b_256(
                [
                    to_local_output.as_slice(),
                    to_local_output_data.as_slice(),
                    to_remote_output.as_slice(),
                    to_remote_output_data.as_slice(),
                    commitment_lock_script_args.as_slice(),
                ]
                .concat(),
            );

            sign_ctx.sign(message.as_slice())?
        };

        // Note that we must update channel state here to update commitment number,
        // so that next step will obtain the correct commitment point.
        self.increment_remote_commitment_number();
        let point = self.get_current_local_commitment_point();

        network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                    self.get_remote_peer_id(),
                    FiberMessage::revoke_and_ack(RevokeAndAck {
                        channel_id: self.get_id(),
                        revocation_partial_signature,
                        commitment_tx_partial_signature,
                        next_per_commitment_point: point,
                    }),
                )),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        Ok(())
    }

    pub fn get_id(&self) -> Hash256 {
        self.id
    }

    pub fn get_local_peer_id(&self) -> PeerId {
        self.local_pubkey.tentacle_peer_id()
    }

    pub fn get_remote_peer_id(&self) -> PeerId {
        self.remote_pubkey.tentacle_peer_id()
    }

    pub fn get_local_secnonce(&self) -> SecNonce {
        self.signer
            .derive_musig2_nonce(self.get_local_commitment_number())
    }

    pub fn get_local_nonce(&self) -> PubNonce {
        self.get_local_secnonce().public_nonce()
    }

    pub fn get_next_local_secnonce(&self) -> SecNonce {
        self.signer
            .derive_musig2_nonce(self.get_next_commitment_number(true))
    }

    pub fn get_next_local_nonce(&self) -> PubNonce {
        self.get_next_local_secnonce().public_nonce()
    }

    pub fn get_remote_nonce(&self) -> PubNonce {
        let comitment_number = self.get_remote_commitment_number();
        assert!(self.remote_nonces.len() <= 2);
        self.remote_nonces
            .iter()
            .rev()
            .find_map(|(number, nonce)| {
                if *number == comitment_number {
                    Some(nonce.clone())
                } else {
                    None
                }
            })
            .expect("get_remote_nonce")
    }

    fn save_remote_nonce(&mut self, nonce: PubNonce) {
        let next_remote_number = if self.remote_nonces.is_empty() {
            0
        } else {
            self.get_remote_commitment_number() + 1
        };
        self.remote_nonces.push((next_remote_number, nonce));
        if self.remote_nonces.len() > 2 {
            self.remote_nonces.remove(0);
        }
    }

    fn save_remote_nonce_for_raa(&mut self) {
        let nonce = self.get_remote_nonce();
        self.last_used_nonce_in_commitment_signed = Some(nonce);
    }

    fn take_remote_nonce_for_raa(&mut self) -> PubNonce {
        self.last_used_nonce_in_commitment_signed
            .take()
            .expect("set last_used_nonce_in_commitment_signed in commitment signed")
    }

    pub fn get_current_commitment_numbers(&self) -> CommitmentNumbers {
        self.commitment_numbers
    }

    pub fn get_local_commitment_number(&self) -> u64 {
        self.commitment_numbers.get_local()
    }

    pub fn get_remote_commitment_number(&self) -> u64 {
        self.commitment_numbers.get_remote()
    }

    fn set_remote_commitment_number(&mut self, number: u64) {
        self.commitment_numbers.remote = number;
    }

    pub fn increment_local_commitment_number(&mut self) {
        self.commitment_numbers.increment_local();
    }

    pub fn increment_remote_commitment_number(&mut self) {
        self.commitment_numbers.increment_remote();
    }

    pub fn get_current_commitment_number(&self, for_remote: bool) -> u64 {
        if for_remote {
            self.get_local_commitment_number()
        } else {
            self.get_remote_commitment_number()
        }
    }

    pub fn get_next_commitment_number(&self, for_remote: bool) -> u64 {
        self.get_current_commitment_number(for_remote) + 1
    }

    pub fn get_next_offering_tlc_id(&self) -> u64 {
        self.tlc_state.get_next_offering()
    }

    pub fn get_next_received_tlc_id(&self) -> u64 {
        self.tlc_state.get_next_received()
    }

    pub fn increment_next_offered_tlc_id(&mut self) {
        self.tlc_state.increment_offering();
    }

    pub fn increment_next_received_tlc_id(&mut self) {
        self.tlc_state.increment_received();
    }

    pub fn get_offered_tlc(&self, tlc_id: u64) -> Option<&AddTlcInfo> {
        self.tlc_state.get(&TLCId::Offered(tlc_id))
    }

    pub fn get_received_tlc(&self, tlc_id: u64) -> Option<&AddTlcInfo> {
        self.tlc_state.get(&TLCId::Received(tlc_id))
    }

    pub(crate) fn set_received_tlc_preimage(&mut self, tlc_id: u64, preimage: Option<Hash256>) {
        if let Some(tlc) = self.tlc_state.get_mut(&TLCId::Received(tlc_id)) {
            tlc.payment_preimage = preimage;
        }
    }

    pub fn check_insert_tlc(&mut self, tlc: &AddTlcInfo) -> Result<(), ProcessingChannelError> {
        let payment_hash = tlc.payment_hash;
        if let Some(tlc) = self
            .tlc_state
            .all_tlcs()
            .find(|tlc| tlc.payment_hash == payment_hash)
        {
            return Err(ProcessingChannelError::RepeatedProcessing(format!(
                "Trying to insert tlc with duplicate payment hash {:?} with tlc {:?}",
                payment_hash, tlc
            )));
        }
        if tlc.is_offered() {
            let sent_tlc_value = self.get_offered_tlc_balance();
            debug_assert!(self.to_local_amount >= sent_tlc_value);
            if sent_tlc_value + tlc.amount > self.to_local_amount {
                return Err(ProcessingChannelError::TlcAmountExceedLimit);
            }
        } else {
            let received_tlc_value = self.get_received_tlc_balance();
            debug_assert!(self.to_remote_amount >= received_tlc_value);
            if received_tlc_value + tlc.amount > self.to_remote_amount {
                debug!(
                    "Adding tlc {:?} with amount {} exceeds remote balance {}",
                    tlc.tlc_id,
                    tlc.amount,
                    self.to_remote_amount - received_tlc_value
                );
                return Err(ProcessingChannelError::TlcAmountExceedLimit);
            }
        }
        Ok(())
    }

    // Remove a tlc with a reason. If the tlc is removed, then the channel
    // balance will be updated accordingly. Otherwise, it is guaranteed that
    // the channel state is not updated.
    pub fn remove_tlc_with_reason(
        &mut self,
        tlc_id: TLCId,
        reason: &RemoveTlcReason,
    ) -> Result<AddTlcInfo, ProcessingChannelError> {
        let removed_at = self.get_current_commitment_numbers();
        let current = self.tlc_state.get(&tlc_id).expect("TLC exists").clone();

        match &current.removed_at {
            Some((current_removed_at, current_remove_reason))
                if current_remove_reason == reason && removed_at == *current_removed_at =>
            {
                debug!("Skipping removing of tlc {:?} as it is already removed at {:?} with the same reason {:?}", tlc_id, removed_at, reason);
                return Err(ProcessingChannelError::RepeatedProcessing(
                    "TLC is already removed".to_string(),
                ));
            }
            Some((current_remove_reason, current_removed_at)) => {
                return Err(ProcessingChannelError::InvalidParameter(
                            format!("Illegally removing the same tlc: {:?} was previously removed at {:?} for {:?}, and trying to remove it again at {:?} for {:?}",
                                tlc_id,  current_removed_at, reason, removed_at, current_remove_reason)));
            }
            None => {
                if let RemoveTlcReason::RemoveTlcFulfill(fulfill) = reason {
                    let filled_payment_hash: Hash256 =
                        current.hash_algorithm.hash(fulfill.payment_preimage).into();
                    if current.payment_hash != filled_payment_hash {
                        return Err(ProcessingChannelError::FinalIncorrectPreimage);
                    }

                    // update balance according to the tlc
                    let (mut to_local_amount, mut to_remote_amount) =
                        (self.to_local_amount, self.to_remote_amount);
                    if current.is_offered() {
                        to_local_amount -= current.amount;
                        to_remote_amount += current.amount;
                    } else {
                        to_local_amount += current.amount;
                        to_remote_amount -= current.amount;
                    }
                    self.to_local_amount = to_local_amount;
                    self.to_remote_amount = to_remote_amount;

                    debug!("Updated local balance to {} and remote balance to {} by removing tlc {:?} with reason {:?}",
                            to_local_amount, to_remote_amount, tlc_id, reason);
                }
                self.tlc_state
                    .apply_remove_tlc(tlc_id, removed_at, reason.clone());
            }
        }

        Ok(current.clone())
    }

    pub fn get_local_channel_public_keys(&self) -> &ChannelBasePublicKeys {
        &self.local_channel_public_keys
    }

    pub fn get_remote_channel_public_keys(&self) -> &ChannelBasePublicKeys {
        self.remote_channel_public_keys
            .as_ref()
            .expect("remote channel public keys exist")
    }

    pub fn must_get_funding_transaction(&self) -> &Transaction {
        self.funding_tx
            .as_ref()
            .expect("Funding transaction is present")
    }

    pub fn get_funding_transaction_outpoint(&self) -> Option<OutPoint> {
        self.funding_tx.as_ref().map(|tx| {
            // By convention, the funding tx output for the channel is the first output.
            OutPoint::new(tx.calc_tx_hash(), 0)
        })
    }

    pub fn must_get_funding_transaction_outpoint(&self) -> OutPoint {
        self.get_funding_transaction_outpoint()
            .expect("Funding transaction outpoint is present")
    }

    pub fn get_funding_transaction_block_number(&self) -> BlockNumber {
        self.funding_tx_confirmed_at
            .expect("funding tx confirmed_at is present")
            .0
    }

    pub fn get_funding_transaction_index(&self) -> u32 {
        self.funding_tx_confirmed_at
            .expect("funding tx confirmed_at is present")
            .1
    }

    pub fn get_local_shutdown_script(&self) -> Script {
        self.local_shutdown_script.clone()
    }

    pub fn get_remote_shutdown_script(&self) -> Script {
        self.remote_shutdown_script
            .as_ref()
            .expect("remote_shutdown_script should be set in current state")
            .clone()
    }

    fn get_local_commitment_point(&self, commitment_number: u64) -> Pubkey {
        self.signer.get_commitment_point(commitment_number)
    }

    /// Get the counterparty commitment point for the given commitment number.
    fn get_remote_commitment_point(&self, commitment_number: u64) -> Pubkey {
        self.remote_commitment_points
            .iter()
            .find_map(|(number, point)| {
                if *number == commitment_number {
                    Some(point.clone())
                } else {
                    None
                }
            })
            .expect("remote commitment point should exist")
    }

    fn get_current_local_commitment_point(&self) -> Pubkey {
        self.get_local_commitment_point(self.get_remote_commitment_number())
    }

    pub fn get_funding_lock_script_xonly_key(&self) -> XOnlyPublicKey {
        let pubkey: secp256k1::PublicKey = self.get_musig2_agg_context().aggregated_pubkey();
        pubkey.into()
    }

    pub fn get_funding_lock_script_xonly(&self) -> [u8; 32] {
        self.get_musig2_agg_context()
            .aggregated_pubkey::<Point>()
            .serialize_xonly()
    }

    pub fn get_funding_lock_script(&self) -> Script {
        let aggregated_pubkey = self.get_funding_lock_script_xonly();
        let pubkey_hash = blake2b_256(aggregated_pubkey);
        get_script_by_contract(Contract::FundingLock, &pubkey_hash[0..20])
    }

    pub fn get_funding_request(&self) -> FundingRequest {
        FundingRequest {
            script: self.get_funding_lock_script(),
            udt_type_script: self.funding_udt_type_script.clone(),
            local_amount: self.to_local_amount,
            funding_fee_rate: self.funding_fee_rate,
            remote_amount: self.to_remote_amount,
            local_reserved_ckb_amount: self.local_reserved_ckb_amount,
            remote_reserved_ckb_amount: self.remote_reserved_ckb_amount,
        }
    }

    pub fn get_musig2_agg_pubkey(&self) -> Pubkey {
        self.get_musig2_agg_context().aggregated_pubkey()
    }

    pub fn get_musig2_agg_context(&self) -> KeyAggContext {
        let local_pubkey = self.get_local_channel_public_keys().funding_pubkey;
        let remote_pubkey = self.get_remote_channel_public_keys().funding_pubkey;
        let keys = self.order_things_for_musig2(local_pubkey, remote_pubkey);
        KeyAggContext::new(keys).expect("Valid pubkeys")
    }

    pub fn get_channel_announcement_musig2_secnonce(&self) -> SecNonce {
        let seckey = blake2b_hash_with_salt(
            self.signer.musig2_base_nonce.as_ref(),
            b"channel_announcement".as_slice(),
        );
        SecNonce::build(seckey).build()
    }

    pub fn get_channel_announcement_musig2_pubnonce(&self) -> PubNonce {
        self.get_channel_announcement_musig2_secnonce()
            .public_nonce()
    }

    pub fn get_local_musig2_secnonce(&self) -> SecNonce {
        self.signer
            .derive_musig2_nonce(self.get_local_commitment_number())
    }

    pub fn get_local_musig2_pubnonce(&self) -> PubNonce {
        self.get_local_musig2_secnonce().public_nonce()
    }

    pub fn get_musig2_agg_pubnonce(&self) -> AggNonce {
        let local_nonce = self.get_local_nonce();
        let remote_nonce = self.get_remote_nonce();
        let nonces = self.order_things_for_musig2(local_nonce, remote_nonce);
        AggNonce::sum(nonces)
    }

    fn get_active_received_tlcs(&self, local_commitment: bool) -> impl Iterator<Item = AddTlcInfo> {
        self.tlc_state
            .get_tlcs_with(local_commitment)
            .into_iter()
            .filter_map(|tlc| match tlc {
                TlcKind::AddTlc(tlc) if tlc.is_received() => Some(tlc),
                _ => None,
            })
    }

    fn get_active_offered_tlcs(&self, local_commitment: bool) -> impl Iterator<Item = AddTlcInfo> {
        self.tlc_state
            .get_tlcs_with(local_commitment)
            .into_iter()
            .filter_map(|tlc| match tlc {
                TlcKind::AddTlc(tlc) if tlc.is_offered() => Some(tlc),
                _ => None,
            })
    }

    // Get the total amount of pending tlcs that are fulfilled
    fn get_pending_fulfilled_tlcs_amount(&self, for_remote: bool, offered: bool) -> u128 {
        let (local_pending_tlcs, remote_pending_tlcs) = if for_remote {
            (
                &self.tlc_state.local_pending_tlcs,
                &self.tlc_state.remote_pending_tlcs,
            )
        } else {
            (
                &self.tlc_state.remote_pending_tlcs,
                &self.tlc_state.local_pending_tlcs,
            )
        };
        let mut pending = local_pending_tlcs
            .get_committed_tlcs()
            .into_iter()
            .chain(remote_pending_tlcs.get_committed_tlcs().into_iter())
            .filter_map(|tlc| match tlc {
                TlcKind::AddTlc(info)
                    if info.removed_at.is_none() && info.is_offered() == offered =>
                {
                    Some((tlc.tlc_id(), info.amount))
                }
                _ => None,
            })
            .collect::<HashMap<TLCId, u128>>();
        let mut fulfilled = 0;

        for tlc in local_pending_tlcs.get_staging_tlcs() {
            match tlc {
                TlcKind::AddTlc(info) => {
                    pending.insert(tlc.tlc_id(), info.amount);
                }
                TlcKind::RemoveTlc(remove_tlc) => {
                    if let Some(amount) = pending.remove(&remove_tlc.tlc_id) {
                        if matches!(remove_tlc.reason, RemoveTlcReason::RemoveTlcFulfill(_)) {
                            fulfilled += amount;
                        }
                    }
                }
            }
        }

        fulfilled
    }

    pub fn get_all_received_tlcs(&self) -> impl Iterator<Item = &AddTlcInfo> {
        self.tlc_state.all_tlcs().filter(|tlc| tlc.is_received())
    }

    pub fn get_all_offer_tlcs(&self) -> impl Iterator<Item = &AddTlcInfo> {
        self.tlc_state.all_tlcs().filter(|tlc| tlc.is_offered())
    }

    // Get the pubkeys for the tlc. Tlc pubkeys are the pubkeys held by each party
    // while this tlc was created (pubkeys are derived from the commitment number
    // when this tlc was created). The pubkeys returned here are sorted.
    // The offerer who offered this tlc will have the first pubkey, and the receiver
    // will have the second pubkey.
    // This tlc must have valid local_committed_at and remote_committed_at fields.
    pub fn get_tlc_pubkeys(&self, tlc: &AddTlcInfo) -> (Pubkey, Pubkey) {
        let is_offered = tlc.is_offered();
        let CommitmentNumbers {
            local: local_commitment_number,
            remote: remote_commitment_number,
        } = tlc.get_commitment_numbers();
        debug!(
            "Local commitment number: {}, remote commitment number: {}",
            local_commitment_number, remote_commitment_number
        );
        let local_pubkey = derive_tlc_pubkey(
            &self.get_local_channel_public_keys().tlc_base_key,
            &self.get_local_commitment_point(remote_commitment_number),
        );
        let remote_pubkey = derive_tlc_pubkey(
            &self.get_remote_channel_public_keys().tlc_base_key,
            &self.get_remote_commitment_point(local_commitment_number),
        );

        if is_offered {
            (local_pubkey, remote_pubkey)
        } else {
            (remote_pubkey, local_pubkey)
        }
    }

    fn get_active_received_tlc_with_pubkeys(
        &self,
        local: bool,
    ) -> Vec<(AddTlcInfo, Pubkey, Pubkey)> {
        self.get_active_received_tlcs(local)
            .map(move |tlc| {
                let (k1, k2) = self.get_tlc_pubkeys(&tlc);
                (tlc, k1, k2)
            })
            .collect()
    }

    fn get_active_offered_tlc_with_pubkeys(
        &self,
        local: bool,
    ) -> Vec<(AddTlcInfo, Pubkey, Pubkey)> {
        self.get_active_offered_tlcs(local)
            .map(move |tlc| {
                let (k1, k2) = self.get_tlc_pubkeys(&tlc);
                (tlc, k1, k2)
            })
            .collect()
    }

    fn get_active_htlcs(&self, local: bool) -> Vec<u8> {
        // Build a sorted array of TLC so that both party can generate the same commitment transaction.
        let tlcs = {
            let (mut received_tlcs, mut offered_tlcs) = (
                self.get_active_received_tlc_with_pubkeys(local),
                self.get_active_offered_tlc_with_pubkeys(local),
            );
            let (mut a, mut b) = if local {
                (received_tlcs, offered_tlcs)
            } else {
                for (tlc, _, _) in received_tlcs.iter_mut().chain(offered_tlcs.iter_mut()) {
                    // Need to flip these fields for the counterparty.
                    tlc.flip_mut();
                }
                (offered_tlcs, received_tlcs)
            };
            a.sort_by(|x, y| u64::from(x.0.tlc_id).cmp(&u64::from(y.0.tlc_id)));
            b.sort_by(|x, y| u64::from(x.0.tlc_id).cmp(&u64::from(y.0.tlc_id)));
            [a, b].concat()
        };
        if tlcs.is_empty() {
            Vec::new()
        } else {
            let mut result = vec![tlcs.len() as u8];
            for (tlc, local, remote) in tlcs {
                result.extend_from_slice(&tlc.get_htlc_type().to_le_bytes());
                result.extend_from_slice(&tlc.amount.to_le_bytes());
                result.extend_from_slice(&tlc.get_hash());
                result.extend_from_slice(&local.serialize());
                result.extend_from_slice(&remote.serialize());
                result.extend_from_slice(
                    &Since::new(SinceType::Timestamp, tlc.expiry, false)
                        .value()
                        .to_le_bytes(),
                );
            }
            result
        }
    }

    fn any_tlc_pending(&self) -> bool {
        self.tlc_state
            .all_tlcs()
            .any(|tlc| tlc.removed_at.is_none())
    }

    pub fn get_local_funding_pubkey(&self) -> &Pubkey {
        &self.get_local_channel_public_keys().funding_pubkey
    }

    pub fn get_remote_funding_pubkey(&self) -> &Pubkey {
        &self.get_remote_channel_public_keys().funding_pubkey
    }

    fn check_valid_to_auto_accept_shutdown(&self) -> bool {
        let Some(remote_fee_rate) = self.remote_shutdown_info.as_ref().map(|i| i.fee_rate) else {
            return false;
        };
        if remote_fee_rate < self.commitment_fee_rate {
            return false;
        }
        let fee = calculate_shutdown_tx_fee(
            remote_fee_rate,
            &self.funding_udt_type_script,
            (
                self.get_remote_shutdown_script(),
                self.get_local_shutdown_script(),
            ),
        );
        let occupied_capacity = match occupied_capacity(
            &self.get_remote_shutdown_script(),
            &self.funding_udt_type_script,
        ) {
            Ok(capacity) => capacity.as_u64(),
            Err(_) => return false,
        };
        let remote_available_max_fee = if self.funding_udt_type_script.is_none() {
            (self.to_remote_amount as u64 + self.remote_reserved_ckb_amount)
                .saturating_sub(occupied_capacity)
        } else {
            self.remote_reserved_ckb_amount
                .saturating_sub(occupied_capacity)
        };
        return fee <= remote_available_max_fee;
    }

    fn check_tlc_expiry(&self, expiry: u64) -> ProcessingChannelResult {
        let current_time = now_timestamp_as_millis_u64();
        if current_time >= expiry {
            debug!(
                "TLC expiry {} is already passed, current time: {}",
                expiry, current_time
            );
            return Err(ProcessingChannelError::TlcExpirySoon);
        }
        if expiry >= current_time + MAX_PAYMENT_TLC_EXPIRY_LIMIT {
            debug!(
                "TLC expiry {} is too far in the future, current time: {}",
                expiry, current_time
            );
            return Err(ProcessingChannelError::TlcExpiryTooFar);
        }
        Ok(())
    }

    // Check whether the reason is valid for removing the tlc.
    fn check_remove_tlc_with_reason(
        &self,
        tlc_id: TLCId,
        reason: &RemoveTlcReason,
    ) -> ProcessingChannelResult {
        if let Some(tlc) = self.tlc_state.get(&tlc_id) {
            if tlc.removed_at.is_some() {
                return Err(ProcessingChannelError::RepeatedProcessing(
                    "TLC is already removed".to_string(),
                ));
            }
            if let RemoveTlcReason::RemoveTlcFulfill(fulfill) = reason {
                let filled_payment_hash: Hash256 =
                    tlc.hash_algorithm.hash(fulfill.payment_preimage).into();
                if tlc.payment_hash != filled_payment_hash {
                    return Err(ProcessingChannelError::FinalIncorrectPreimage);
                }
            }
            Ok(())
        } else {
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "Trying to remove non-existing tlc with id {:?}",
                tlc_id
            )));
        }
    }

    fn check_for_tlc_update(
        &self,
        add_tlc_amount: Option<u128>,
        is_tlc_command_message: bool,
        is_sent: bool,
    ) -> ProcessingChannelResult {
        if is_tlc_command_message && self.tlc_state.waiting_ack {
            return Err(ProcessingChannelError::WaitingTlcAck);
        }
        match self.state {
            ChannelState::ChannelReady() => {}
            ChannelState::ShuttingDown(_) if add_tlc_amount.is_none() => {}
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Invalid state {:?} for {} tlc",
                    self.state,
                    if add_tlc_amount.is_some() {
                        "adding"
                    } else {
                        "removing"
                    }
                )))
            }
        }

        if let Some(add_amount) = add_tlc_amount {
            self.check_tlc_limits(add_amount, is_sent)?;
        }
        Ok(())
    }

    fn check_tlc_limits(
        &self,
        add_amount: u128,
        is_sent: bool,
    ) -> Result<(), ProcessingChannelError> {
        if add_amount == 0 {
            return Err(ProcessingChannelError::TlcAmountIsTooLow);
        }
        if is_sent {
            let active_offered_tls_number = self.get_all_offer_tlcs().count() as u64 + 1;
            if active_offered_tls_number > self.local_constraints.max_tlc_number_in_flight {
                return Err(ProcessingChannelError::TlcNumberExceedLimit);
            }

            let active_offered_amount = self
                .get_all_offer_tlcs()
                .fold(0_u128, |sum, tlc| sum + tlc.amount)
                + add_amount;
            if active_offered_amount > self.local_constraints.max_tlc_value_in_flight {
                return Err(ProcessingChannelError::TlcValueInflightExceedLimit);
            }
        } else {
            let active_received_tls_number = self.get_all_received_tlcs().count() as u64 + 1;
            if active_received_tls_number > self.remote_constraints.max_tlc_number_in_flight {
                return Err(ProcessingChannelError::TlcNumberExceedLimit);
            }

            let active_received_amount = self
                .get_all_received_tlcs()
                .fold(0_u128, |sum, tlc| sum + tlc.amount)
                + add_amount;
            if active_received_amount > self.remote_constraints.max_tlc_value_in_flight {
                return Err(ProcessingChannelError::TlcValueInflightExceedLimit);
            }
        }

        Ok(())
    }

    fn create_outbounding_tlc(&self, command: AddTlcCommand) -> AddTlcInfo {
        let id = self.get_next_offering_tlc_id();
        assert!(
            self.get_offered_tlc(id).is_none(),
            "Must not have the same id in pending offered tlcs"
        );

        AddTlcInfo {
            channel_id: self.get_id(),
            tlc_id: TLCId::Offered(id),
            amount: command.amount,
            payment_hash: command.payment_hash,
            expiry: command.expiry,
            hash_algorithm: command.hash_algorithm,
            created_at: self.get_current_commitment_numbers(),
            payment_preimage: None,
            removed_at: None,
            onion_packet: command.onion_packet,
            shared_secret: command.shared_secret,
            previous_tlc: command
                .previous_tlc
                .map(|(channel_id, tlc_id)| (channel_id, TLCId::Received(tlc_id))),
        }
    }

    fn create_inbounding_tlc(&self, message: AddTlc) -> Result<AddTlcInfo, ProcessingChannelError> {
        let tlc_info = AddTlcInfo {
            tlc_id: TLCId::Received(message.tlc_id),
            channel_id: self.get_id(),
            amount: message.amount,
            payment_hash: message.payment_hash,
            expiry: message.expiry,
            hash_algorithm: message.hash_algorithm,
            // will be set when apply AddTlc operations after the signature is checked
            onion_packet: message.onion_packet,
            // No need to save shared secret for inbound TLC.
            shared_secret: NO_SHARED_SECRET.clone(),
            created_at: self.get_current_commitment_numbers(),
            payment_preimage: None,
            removed_at: None,
            previous_tlc: None,
        };
        Ok(tlc_info)
    }

    fn aggregate_partial_signatures_to_consume_funding_cell(
        &self,
        partial_signatures: [PartialSignature; 2],
        tx: &TransactionView,
    ) -> Result<TransactionView, ProcessingChannelError> {
        let verify_ctx = Musig2VerifyContext::from(self);
        let signature = verify_ctx
            .aggregate_partial_signatures_for_msg(partial_signatures, tx.hash().as_slice())?;

        let witness =
            create_witness_for_funding_cell(self.get_funding_lock_script_xonly(), signature);
        Ok(tx
            .as_advanced_builder()
            .set_witnesses(vec![witness.pack()])
            .build())
    }

    fn complete_partially_signed_tx(
        &self,
        psct: &PartiallySignedCommitmentTransaction,
    ) -> Result<(TransactionView, SettlementData), ProcessingChannelError> {
        let completed_commitment_tx = {
            let sign_ctx = Musig2SignContext::from(self);
            let our_funding_tx_partial_signature =
                sign_ctx.sign(psct.commitment_tx.hash().as_slice())?;
            self.aggregate_partial_signatures_to_consume_funding_cell(
                [
                    psct.funding_tx_partial_signature,
                    our_funding_tx_partial_signature,
                ],
                &psct.commitment_tx,
            )?
        };

        let settlement_data = {
            let settlement_tx = &psct.settlement_tx;
            let commitment_tx = &psct.commitment_tx;
            let to_local_output = settlement_tx
                .outputs()
                .get(0)
                .expect("get output 0 of settlement tx");
            let to_local_output_data = settlement_tx
                .outputs_data()
                .get(0)
                .expect("get output 0 data of settlement tx");
            let to_remote_output = settlement_tx
                .outputs()
                .get(1)
                .expect("get output 1 of settlement tx");
            let to_remote_output_data = settlement_tx
                .outputs_data()
                .get(1)
                .expect("get output 1 data of settlement tx");
            let args = commitment_tx
                .outputs()
                .get(0)
                .expect("get output 0 of commitment tx")
                .lock()
                .args()
                .raw_data();
            let message = blake2b_256(
                [
                    to_local_output.as_slice(),
                    to_local_output_data.as_slice(),
                    to_remote_output.as_slice(),
                    to_remote_output_data.as_slice(),
                    &args[0..36],
                ]
                .concat(),
            );
            let sign_ctx = Musig2SignContext::from((self, false));
            let our_commitment_tx_partial_signature = sign_ctx.sign(message.as_slice())?;

            let verify_ctx = Musig2VerifyContext::from((self, false));
            let aggregated_signature = verify_ctx.aggregate_partial_signatures_for_msg(
                [
                    our_commitment_tx_partial_signature,
                    psct.commitment_tx_partial_signature,
                ],
                message.as_slice(),
            )?;
            let x_only_aggregated_pubkey = self.get_commitment_lock_script_xonly(false);

            SettlementData {
                x_only_aggregated_pubkey,
                aggregated_signature,
                to_local_output,
                to_local_output_data,
                to_remote_output,
                to_remote_output_data,
            }
        };

        Ok((completed_commitment_tx, settlement_data))
    }

    fn maybe_transition_to_shutdown(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        // This function will also be called when we resolve all pending tlcs.
        // If we are not in the ShuttingDown state, we should not do anything.
        let flags = match self.state {
            ChannelState::ShuttingDown(flags) => flags,
            _ => {
                return Ok(());
            }
        };

        if !flags.contains(ShuttingDownFlags::AWAITING_PENDING_TLCS) || self.any_tlc_pending() {
            debug!(
                "Will not shutdown the channel because we require all tlcs resolved and both parties sent the Shutdown message, current state: {:?}, pending tlcs: {:?}",
                &self.state,
                &self.tlc_state.all_commited_tlcs().collect::<Vec<_>>()
            );
            return Ok(());
        }

        debug!("All pending tlcs are resolved, transitioning to Shutdown state");
        self.update_state(ChannelState::ShuttingDown(
            flags | ShuttingDownFlags::DROPPING_PENDING,
        ));

        if self.local_shutdown_info.is_some() && self.remote_shutdown_info.is_some() {
            let shutdown_tx = self.build_shutdown_tx()?;
            let sign_ctx = Musig2SignContext::from(&*self);

            let local_shutdown_info = self
                .local_shutdown_info
                .as_mut()
                .expect("local shudown info exists");
            let remote_shutdown_info = self
                .remote_shutdown_info
                .as_ref()
                .expect("remote shudown info exists");
            let shutdown_scripts = (
                local_shutdown_info.close_script.clone(),
                remote_shutdown_info.close_script.clone(),
            );
            let local_shutdown_signature = match local_shutdown_info.signature {
                Some(signature) => signature,
                None => {
                    let signature = sign_ctx.sign(shutdown_tx.hash().as_slice())?;
                    local_shutdown_info.signature = Some(signature);

                    network
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                                self.get_remote_peer_id(),
                                FiberMessage::closing_signed(ClosingSigned {
                                    partial_signature: signature,
                                    channel_id: self.get_id(),
                                }),
                            )),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    signature
                }
            };

            if let Some(remote_shutdown_signature) = remote_shutdown_info.signature {
                let tx: TransactionView = self
                    .aggregate_partial_signatures_to_consume_funding_cell(
                        [local_shutdown_signature, remote_shutdown_signature],
                        &shutdown_tx,
                    )?;
                assert_eq!(
                    tx.data().serialized_size_in_block(),
                    shutdown_tx_size(&self.funding_udt_type_script, shutdown_scripts)
                );

                self.update_state(ChannelState::Closed(CloseFlags::COOPERATIVE));

                network
                    .send_message(NetworkActorMessage::new_event(
                        NetworkActorEvent::ClosingTransactionPending(
                            self.get_id(),
                            self.get_remote_peer_id(),
                            tx,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            } else {
                debug!("We have sent our shutdown signature, waiting for counterparty's signature");
            }
        } else {
            debug!("Not ready to shutdown the channel, waiting for both parties to send the Shutdown message");
        }

        Ok(())
    }

    fn handle_accept_channel_message(
        &mut self,
        accept_channel: AcceptChannel,
    ) -> ProcessingChannelResult {
        if self.state != ChannelState::NegotiatingFunding(NegotiatingFundingFlags::OUR_INIT_SENT) {
            return Err(ProcessingChannelError::InvalidState(format!(
                "accepting a channel while in state {:?}, expecting NegotiatingFundingFlags::OUR_INIT_SENT",
                self.state
            )));
        }

        self.update_state(ChannelState::NegotiatingFunding(
            NegotiatingFundingFlags::INIT_SENT,
        ));

        self.to_remote_amount = accept_channel.funding_amount;
        self.remote_reserved_ckb_amount = accept_channel.reserved_ckb_amount;

        self.save_remote_nonce(accept_channel.next_local_nonce.clone());
        let remote_pubkeys = (&accept_channel).into();
        self.remote_channel_public_keys = Some(remote_pubkeys);
        self.remote_commitment_points = vec![
            (0, accept_channel.first_per_commitment_point),
            (1, accept_channel.second_per_commitment_point),
        ];
        self.remote_shutdown_script = Some(accept_channel.shutdown_script.clone());

        self.remote_constraints = ChannelConstraints::new(
            accept_channel.max_tlc_value_in_flight,
            accept_channel.max_tlc_number_in_flight,
        );

        self.check_accept_channel_parameters()?;

        match accept_channel.channel_announcement_nonce {
            Some(ref nonce) if self.is_public() => {
                debug!("Updating remote channel announcement nonce: {:?}", nonce);
                self.update_remote_channel_announcement_nonce(nonce);
            }
            None if !self.is_public() => {}
            _ => {
                return Err(ProcessingChannelError::InvalidParameter(format!(
                    "Must/Mustn't send announcement nonce if channel is public/private, nonce {:?}, channel is public: {}",
                    &accept_channel.channel_announcement_nonce, self.is_public()
                )));
            }
        }
        debug!(
            "Successfully processed AcceptChannel message {:?}",
            &accept_channel
        );
        Ok(())
    }

    // This is the dual of `handle_tx_collaboration_command`. Any logic error here is likely
    // to present in the other function as well.
    fn handle_tx_collaboration_msg(
        &mut self,
        msg: TxCollaborationMsg,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        debug!("Processing tx collaboration message: {:?}", &msg);
        let is_complete_message = matches!(msg, TxCollaborationMsg::TxComplete(_));
        let is_waiting_for_remote = match self.state {
            ChannelState::CollaboratingFundingTx(flags) => {
                flags.contains(CollaboratingFundingTxFlags::AWAITING_REMOTE_TX_COLLABORATION_MSG)
            }
            _ => false,
        };
        let flags = match self.state {
            // Starting transaction collaboration
            ChannelState::NegotiatingFunding(NegotiatingFundingFlags::INIT_SENT)
                if !self.is_acceptor =>
            {
                return Err(ProcessingChannelError::InvalidState(
                    "Initiator received a tx collaboration message".to_string(),
                ));
            }
            ChannelState::NegotiatingFunding(_) => {
                debug!("Started negotiating funding tx collaboration, and transitioning from {:?} to CollaboratingFundingTx state", self.state);
                self.state =
                    ChannelState::CollaboratingFundingTx(CollaboratingFundingTxFlags::empty());
                CollaboratingFundingTxFlags::empty()
            }
            ChannelState::CollaboratingFundingTx(_)
                if !is_complete_message && !is_waiting_for_remote =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Trying to process message {:?} while in {:?} (should only receive non-complete message after sent response from peer)",
                    &msg, self.state
                )));
            }
            ChannelState::CollaboratingFundingTx(flags) => {
                if flags.contains(CollaboratingFundingTxFlags::THEIR_TX_COMPLETE_SENT) {
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Received a tx collaboration message {:?}, but we are already in the state {:?} where the remote has sent a complete message",
                        &msg, &self.state
                    )));
                }
                debug!(
                    "Processing tx collaboration message {:?} for state {:?}",
                    &msg, &self.state
                );
                flags
            }
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Invalid tx collaboration message {:?} for state {:?}",
                    &msg, &self.state
                )));
            }
        };
        match msg {
            TxCollaborationMsg::TxUpdate(msg) => {
                // TODO check if the tx is valid.
                self.funding_tx = Some(msg.tx.clone());
                if self.is_tx_final(&msg.tx)? {
                    self.maybe_complete_tx_collaboration(msg.tx, network)?;
                } else {
                    network
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::UpdateChannelFunding(
                                self.get_id(),
                                msg.tx,
                                self.get_funding_request(),
                            ),
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    self.update_state(ChannelState::CollaboratingFundingTx(
                        CollaboratingFundingTxFlags::PREPARING_LOCAL_TX_COLLABORATION_MSG,
                    ));
                }
            }
            TxCollaborationMsg::TxComplete(tx_complete) => {
                self.check_tx_complete_preconditions()?;
                let settlement_data = self.check_init_commitment_tx_signature(
                    tx_complete.commitment_tx_partial_signature,
                )?;
                network
                    .send_message(NetworkActorMessage::new_notification(
                        NetworkServiceEvent::RemoteTxComplete(
                            self.get_remote_peer_id(),
                            self.get_id(),
                            self.get_funding_lock_script(),
                            settlement_data,
                        ),
                    ))
                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                let flags = flags | CollaboratingFundingTxFlags::THEIR_TX_COMPLETE_SENT;
                self.update_state(ChannelState::CollaboratingFundingTx(flags));
            }
        }
        Ok(())
    }

    fn verify_commitment_signed_and_send_ack(
        &mut self,
        commitment_signed: CommitmentSigned,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        let flags = match self.state {
            ChannelState::CollaboratingFundingTx(flags)
                if !flags.contains(CollaboratingFundingTxFlags::COLLABRATION_COMPLETED) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed message in state {:?}, as collaboration is not completed yet.",
                    &self.state
                )));
            }
            ChannelState::CollaboratingFundingTx(_) => {
                CommitmentSignedFlags::SigningCommitment(SigningCommitmentFlags::empty())
            }
            ChannelState::SigningCommitment(flags)
                if flags.contains(SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT) =>
            {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to process commitment_signed message in state {:?}, as we have already received our commitment_signed message.",
                    &self.state
                )));
            }
            ChannelState::SigningCommitment(flags) => {
                CommitmentSignedFlags::SigningCommitment(flags)
            }
            ChannelState::ChannelReady() => CommitmentSignedFlags::ChannelReady(),
            ChannelState::ShuttingDown(flags) => {
                if flags.contains(ShuttingDownFlags::AWAITING_PENDING_TLCS) {
                    debug!(
                        "Signing commitment transactions while shutdown is pending, current state {:?}",
                        &self.state
                    );
                    CommitmentSignedFlags::PendingShutdown()
                } else {
                    return Err(ProcessingChannelError::InvalidState(format!(
                        "Unable to process commitment_signed message in shutdowning state with flags {:?}",
                        &flags
                    )));
                }
            }
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to send commitment signed message in state {:?}",
                    &self.state
                )));
            }
        };

        let (commitment_tx, settlement_data) = self.verify_and_complete_tx(
            commitment_signed.funding_tx_partial_signature,
            commitment_signed.commitment_tx_partial_signature,
        )?;

        // Notify outside observers.
        network
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::RemoteCommitmentSigned(
                    self.get_remote_peer_id(),
                    self.get_id(),
                    commitment_tx.clone(),
                    settlement_data,
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);

        self.save_remote_nonce(commitment_signed.next_local_nonce);
        self.latest_commitment_transaction = Some(commitment_tx.data());
        match flags {
            CommitmentSignedFlags::SigningCommitment(flags) => {
                let flags = flags | SigningCommitmentFlags::THEIR_COMMITMENT_SIGNED_SENT;
                self.update_state(ChannelState::SigningCommitment(flags));
                self.maybe_transition_to_tx_signatures(flags, network)?;
            }
            CommitmentSignedFlags::ChannelReady() | CommitmentSignedFlags::PendingShutdown() => {
                self.send_revoke_and_ack_message(network)?;
                match flags {
                    CommitmentSignedFlags::ChannelReady() => {}
                    CommitmentSignedFlags::PendingShutdown() => {
                        // TODO: Handle error in the below function call.
                        // We've already updated our state, we should never fail here.
                        self.maybe_transition_to_shutdown(network)?;
                    }
                    _ => {
                        unreachable!(
                            "Invalid flags for commitment signed message, should have handled {:?}",
                            flags
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn maybe_transition_to_tx_signatures(
        &mut self,
        flags: SigningCommitmentFlags,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        if flags.contains(SigningCommitmentFlags::COMMITMENT_SIGNED_SENT) {
            debug!("Commitment signed message sent by both sides, tranitioning to AwaitingTxSignatures state");
            self.update_state(ChannelState::AwaitingTxSignatures(
                AwaitingTxSignaturesFlags::empty(),
            ));
            if self.should_local_send_tx_signatures_first() {
                debug!("It is our turn to send tx_signatures, so we will do it now.");
                self.handle_tx_signatures(network, None)?;
            }
        }
        Ok(())
    }

    // TODO: currently witnesses in the tx_signatures molecule message are a list of bytes.
    // It is unclear how can we compose two partial sets witnesses into a complete
    // set of witnesses.
    fn handle_tx_signatures(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
        // If partial_witnesses is given, then it is the counterparty that send a message
        // to us, and we must combine them to make a full list of witnesses.
        // Otherwise, we are the one who is to start send the tx_signatures.
        // We can just create a partial set of witnesses, and sent them to the peer.
        partial_witnesses: Option<Vec<Vec<u8>>>,
    ) -> ProcessingChannelResult {
        let flags = match self.state {
            ChannelState::AwaitingTxSignatures(flags)
                if flags.contains(AwaitingTxSignaturesFlags::THEIR_TX_SIGNATURES_SENT)
                    && partial_witnesses.is_some() =>
            {
                return Err(ProcessingChannelError::RepeatedProcessing(format!(
                    "tx_signatures partial witnesses {:?}",
                    partial_witnesses
                )));
            }
            ChannelState::AwaitingTxSignatures(flags)
                if flags.contains(AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT)
                    && partial_witnesses.is_none() =>
            {
                return Err(ProcessingChannelError::RepeatedProcessing(
                    "We have already sent our tx_signatures".to_string(),
                ));
            }
            ChannelState::SigningCommitment(flags)
                if flags.contains(SigningCommitmentFlags::COMMITMENT_SIGNED_SENT) =>
            {
                AwaitingTxSignaturesFlags::empty()
            }
            ChannelState::AwaitingTxSignatures(flags) => flags,
            _ => {
                return Err(ProcessingChannelError::InvalidState(format!(
                    "Unable to build and sign funding tx in state {:?}",
                    &self.state
                )));
            }
        };

        let flags = if partial_witnesses.is_some() {
            flags | AwaitingTxSignaturesFlags::THEIR_TX_SIGNATURES_SENT
        } else {
            flags | AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT
        };
        self.update_state(ChannelState::AwaitingTxSignatures(flags));

        let funding_tx = self
            .funding_tx
            .clone()
            .ok_or(ProcessingChannelError::InvalidState(
                "Funding transaction is not present".to_string(),
            ))?;

        network
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SignTx(
                    self.get_remote_peer_id(),
                    self.get_id(),
                    funding_tx,
                    partial_witnesses,
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        let flags = flags | AwaitingTxSignaturesFlags::OUR_TX_SIGNATURES_SENT;
        self.update_state(ChannelState::AwaitingTxSignatures(flags));

        Ok(())
    }

    async fn maybe_public_channel_is_ready(&mut self, network: &ActorRef<NetworkActorMessage>) {
        debug!("Trying to create channel announcement message for public channel");
        if let Some((channel_announcement, channel_update)) =
            self.try_create_channel_messages(network).await
        {
            debug!(
                "Channel announcement/update message for {:?} created, public channel is ready",
                self.get_id(),
            );
            self.on_channel_ready(network).await;

            debug!(
                "Broadcasting channel announcement {:?} and channel update {:?}",
                &channel_announcement, &channel_update
            );
            network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::BroadcastMessages(vec![
                        BroadcastMessage::ChannelAnnouncement(channel_announcement),
                        BroadcastMessage::ChannelUpdate(channel_update),
                    ]),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);

            // Note that there is a racing condition here. The peer may have not finished
            // generating the channel update message yet. In order to reliably query the
            // peer for the channel update message, we may to retry the query a few times.
            let peer_id = self.get_remote_peer_id();
            let queries = if self.local_is_node1() {
                vec![
                    BroadcastMessageQuery {
                        channel_outpoint: self.must_get_funding_transaction_outpoint(),
                        flags: BroadcastMessageQueryFlags::ChannelUpdateOfNode2,
                    },
                    BroadcastMessageQuery {
                        channel_outpoint: self.must_get_funding_transaction_outpoint(),
                        flags: BroadcastMessageQueryFlags::NodeAnnouncementNode2,
                    },
                ]
            } else {
                vec![
                    BroadcastMessageQuery {
                        channel_outpoint: self.must_get_funding_transaction_outpoint(),
                        flags: BroadcastMessageQueryFlags::ChannelUpdateOfNode1,
                    },
                    BroadcastMessageQuery {
                        channel_outpoint: self.must_get_funding_transaction_outpoint(),
                        flags: BroadcastMessageQueryFlags::NodeAnnouncementNode1,
                    },
                ]
            };
            debug!(
                "Querying for channel update and node announcement messages from {:?}",
                &peer_id
            );
            network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::QueryBroadcastMessages(peer_id, queries),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        }
    }

    async fn maybe_channel_is_ready(&mut self, network: &ActorRef<NetworkActorMessage>) {
        match self.state {
            ChannelState::AwaitingChannelReady(flags) => {
                if flags.contains(AwaitingChannelReadyFlags::CHANNEL_READY) {
                    if !self.is_public() {
                        self.on_channel_ready(network).await;
                    } else {
                        self.maybe_public_channel_is_ready(network).await;
                    }
                }
            }
            _ => {
                panic!(
                    "Invalid state {:?} for maybe_on_channel_ready (expected AwaitingChannelReady)",
                    &self.state
                );
            }
        }
    }

    async fn on_channel_ready(&mut self, network: &ActorRef<NetworkActorMessage>) {
        self.update_state(ChannelState::ChannelReady());
        self.increment_local_commitment_number();
        self.increment_remote_commitment_number();
        let peer_id = self.get_remote_peer_id();
        network
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::ChannelReady(
                    self.get_id(),
                    peer_id.clone(),
                    self.must_get_funding_transaction_outpoint(),
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }

    fn append_remote_commitment_point(&mut self, commitment_point: Pubkey) {
        self.remote_commitment_points
            .push((self.get_local_commitment_number(), commitment_point));

        let len = self.remote_commitment_points.len();
        if len > (self.local_constraints.max_tlc_number_in_flight + 1) as usize {
            let min_remote_commitment = self
                .tlc_state
                .all_tlcs()
                .map(|x| x.created_at.remote)
                .min()
                .unwrap_or_default();
            self.remote_commitment_points
                .retain(|(num, _)| *num >= min_remote_commitment);
        }
        assert!(
            self.remote_commitment_points.len()
                <= (self.local_constraints.max_tlc_number_in_flight + 1) as usize
        );
    }

    fn handle_revoke_and_ack_peer_message(
        &mut self,
        network: &ActorRef<NetworkActorMessage>,
        revoke_and_ack: RevokeAndAck,
    ) -> ProcessingChannelResult {
        let RevokeAndAck {
            channel_id: _,
            revocation_partial_signature,
            commitment_tx_partial_signature,
            next_per_commitment_point,
        } = revoke_and_ack;

        let key_agg_ctx = {
            let local_pubkey = self.get_local_channel_public_keys().funding_pubkey;
            let remote_pubkey = self.get_remote_channel_public_keys().funding_pubkey;
            KeyAggContext::new([local_pubkey, remote_pubkey]).expect("Valid pubkeys")
        };
        let x_only_aggregated_pubkey = key_agg_ctx.aggregated_pubkey::<Point>().serialize_xonly();
        let (verify_ctx, sign_ctx) = {
            let local_nonce = self.get_local_nonce();
            let remote_nonce = self.take_remote_nonce_for_raa();
            let nonces = [remote_nonce.clone(), local_nonce];
            let agg_nonce = AggNonce::sum(nonces);

            (
                Musig2VerifyContext {
                    key_agg_ctx: key_agg_ctx.clone(),
                    agg_nonce: agg_nonce.clone(),
                    pubkey: *self.get_remote_funding_pubkey(),
                    pubnonce: remote_nonce,
                },
                Musig2SignContext {
                    key_agg_ctx,
                    agg_nonce,
                    seckey: self.signer.funding_key.clone(),
                    secnonce: self.get_local_musig2_secnonce(),
                },
            )
        };

        let revocation_data = {
            let commitment_tx_fee = calculate_commitment_tx_fee(
                self.commitment_fee_rate,
                &self.funding_udt_type_script,
            );
            let lock_script = self.get_local_shutdown_script();
            let (output, output_data) = if let Some(udt_type_script) = &self.funding_udt_type_script
            {
                let capacity = self.get_total_reserved_ckb_amount() - commitment_tx_fee;
                let output = CellOutput::new_builder()
                    .lock(lock_script)
                    .type_(Some(udt_type_script.clone()).pack())
                    .capacity(capacity.pack())
                    .build();

                let output_data = self.get_total_udt_amount().to_le_bytes().pack();
                (output, output_data)
            } else {
                let capacity = self.get_total_ckb_amount() - commitment_tx_fee;
                let output = CellOutput::new_builder()
                    .lock(lock_script)
                    .capacity(capacity.pack())
                    .build();
                let output_data = Bytes::default();
                (output, output_data)
            };

            let commitment_number = self.get_local_commitment_number() - 1;

            let commitment_lock_script_args = [
                &blake2b_256(x_only_aggregated_pubkey)[0..20],
                self.get_delay_epoch_as_lock_args_bytes().as_slice(),
                commitment_number.to_be_bytes().as_slice(),
            ]
            .concat();

            let message = blake2b_256(
                [
                    output.as_slice(),
                    output_data.as_slice(),
                    commitment_lock_script_args.as_slice(),
                ]
                .concat(),
            );
            verify_ctx.verify(revocation_partial_signature, message.as_slice())?;
            let our_signature = sign_ctx.clone().sign(message.as_slice())?;
            let aggregated_signature = verify_ctx.aggregate_partial_signatures_for_msg(
                [revocation_partial_signature, our_signature],
                message.as_slice(),
            )?;
            RevocationData {
                commitment_number,
                x_only_aggregated_pubkey,
                aggregated_signature,
                output,
                output_data,
            }
        };

        let settlement_data = {
            let (
                [to_local_output, to_remote_output],
                [to_local_output_data, to_remote_output_data],
            ) = self.build_settlement_transaction_outputs(true);
            let commitment_lock_script_args = [
                &blake2b_256(x_only_aggregated_pubkey)[0..20],
                self.get_delay_epoch_as_lock_args_bytes().as_slice(),
                self.get_local_commitment_number().to_be_bytes().as_slice(),
            ]
            .concat();
            let message = blake2b_256(
                [
                    to_local_output.as_slice(),
                    to_local_output_data.as_slice(),
                    to_remote_output.as_slice(),
                    to_remote_output_data.as_slice(),
                    commitment_lock_script_args.as_slice(),
                ]
                .concat(),
            );
            verify_ctx.verify(commitment_tx_partial_signature, message.as_slice())?;
            let our_signature = sign_ctx.sign(message.as_slice())?;
            let aggregated_signature = verify_ctx.aggregate_partial_signatures_for_msg(
                [commitment_tx_partial_signature, our_signature],
                message.as_slice(),
            )?;

            SettlementData {
                x_only_aggregated_pubkey,
                aggregated_signature,
                to_local_output,
                to_local_output_data,
                to_remote_output,
                to_remote_output_data,
            }
        };

        self.increment_local_commitment_number();
        self.append_remote_commitment_point(next_per_commitment_point);

        let staging_tlcs = self.tlc_state.commit_local_tlcs();
        for tlc in staging_tlcs {
            if let TlcKind::RemoveTlc(remove_tlc) = tlc {
                self.remove_tlc_with_reason(remove_tlc.tlc_id, &remove_tlc.reason)
                    .expect("expect remove tlc successfully");
            }
        }
        self.tlc_state.set_waiting_ack(false);

        network
            .send_message(NetworkActorMessage::new_notification(
                NetworkServiceEvent::RevokeAndAckReceived(
                    self.get_remote_peer_id(),
                    self.get_id(),
                    revocation_data,
                    settlement_data,
                ),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
        Ok(())
    }

    fn handle_reestablish_channel_message(
        &mut self,
        reestablish_channel: &ReestablishChannel,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        debug!(
            "Handling reestablish channel message: {:?}, our commitment_numbers {:?}",
            reestablish_channel, self.commitment_numbers,
        );
        self.reestablishing = false;
        match self.state {
            ChannelState::NegotiatingFunding(_flags) => {
                // TODO: in current implementation, we don't store the channel when we are in NegotiatingFunding state.
                // This is an unreachable state for reestablish channel message. we may need to handle this case in the future.
            }
            ChannelState::ChannelReady() => {
                let expected_local_commitment_number = self.get_local_commitment_number();
                let acutal_local_commitment_number = reestablish_channel.remote_commitment_number;
                if acutal_local_commitment_number == expected_local_commitment_number {
                    // resend AddTlc, RemoveTlc and CommitmentSigned messages if needed
                    let mut need_resend_commitment_signed = false;
                    for info in self.tlc_state.all_tlcs() {
                        if info.is_offered() {
                            if info.created_at.get_local() >= acutal_local_commitment_number {
                                // resend AddTlc message
                                network
                                    .send_message(NetworkActorMessage::new_command(
                                        NetworkActorCommand::SendFiberMessage(
                                            FiberMessageWithPeerId::new(
                                                self.get_remote_peer_id(),
                                                FiberMessage::add_tlc(AddTlc {
                                                    channel_id: self.get_id(),
                                                    tlc_id: info.tlc_id.into(),
                                                    amount: info.amount,
                                                    payment_hash: info.payment_hash,
                                                    expiry: info.expiry,
                                                    hash_algorithm: info.hash_algorithm,
                                                    onion_packet: info.onion_packet.clone(),
                                                }),
                                            ),
                                        ),
                                    ))
                                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                                need_resend_commitment_signed = true;
                            }
                        } else if let Some((commitment_number, remove_reason)) = &info.removed_at {
                            if commitment_number.get_local() >= acutal_local_commitment_number {
                                // resend RemoveTlc message
                                network
                                    .send_message(NetworkActorMessage::new_command(
                                        NetworkActorCommand::SendFiberMessage(
                                            FiberMessageWithPeerId::new(
                                                self.get_remote_peer_id(),
                                                FiberMessage::remove_tlc(RemoveTlc {
                                                    channel_id: self.get_id(),
                                                    tlc_id: info.tlc_id.into(),
                                                    reason: remove_reason.clone(),
                                                }),
                                            ),
                                        ),
                                    ))
                                    .expect(ASSUME_NETWORK_ACTOR_ALIVE);

                                need_resend_commitment_signed = true;
                            }
                        }
                    }
                    if need_resend_commitment_signed {
                        debug!("Resend CommitmentSigned message");
                        network
                            .send_message(NetworkActorMessage::new_command(
                                NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                                    channel_id: self.get_id(),
                                    command: ChannelCommand::CommitmentSigned(),
                                }),
                            ))
                            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                    }
                } else if acutal_local_commitment_number == expected_local_commitment_number + 1 {
                    // wait for remote to resend the RevokeAndAck message, do nothing here
                } else {
                    // unreachable state, just log an error for potential bugs
                    error!(
                        "Reestablish channel message with invalid local commitment number: expected {}, actual {}",
                        expected_local_commitment_number, acutal_local_commitment_number
                    );
                }

                let expected_remote_commitment_number = self.get_remote_commitment_number();
                let acutal_remote_commitment_number = reestablish_channel.local_commitment_number;
                if expected_remote_commitment_number == acutal_remote_commitment_number {
                    // synced with remote, do nothing
                } else if expected_remote_commitment_number == acutal_remote_commitment_number + 1 {
                    // Resetting our remote commitment number to the actual remote commitment number
                    // and resend the RevokeAndAck message.
                    self.set_remote_commitment_number(acutal_remote_commitment_number);
                    self.send_revoke_and_ack_message(network)?;
                } else {
                    // unreachable state, just log an error for potential bugs
                    error!(
                        "Reestablish channel message with invalid remote commitment number: expected {}, actual {}",
                        expected_remote_commitment_number, acutal_remote_commitment_number
                    );
                }
            }
            _ => {
                // TODO: @quake we need to handle other states.
                warn!(
                    "Unhandled reestablish channel message in state {:?}",
                    &self.state
                );
            }
        }
        Ok(())
    }

    fn is_tx_final(&self, tx: &Transaction) -> Result<bool, ProcessingChannelError> {
        // TODO: check if the tx is valid
        let tx = tx.clone().into_view();

        let first_output = tx
            .outputs()
            .get(0)
            .ok_or(ProcessingChannelError::InvalidParameter(
                "Funding transaction should have at least one output".to_string(),
            ))?;

        if first_output.lock() != self.get_funding_lock_script() {
            return Err(ProcessingChannelError::InvalidState(
                "Invalid funding transation lock script".to_string(),
            ));
        }

        let current_capacity: u64 = first_output.capacity().unpack();

        // make sure both parties have paid the reserved ckb amount
        if current_capacity <= self.local_reserved_ckb_amount
            || current_capacity <= self.remote_reserved_ckb_amount
        {
            return Ok(false);
        }

        if self.funding_udt_type_script.is_some() {
            let (_output, data) =
                tx.output_with_data(0)
                    .ok_or(ProcessingChannelError::InvalidParameter(
                        "Funding transaction should have at least one output".to_string(),
                    ))?;
            assert!(data.as_ref().len() >= 16);
            let mut amount_bytes = [0u8; 16];
            amount_bytes.copy_from_slice(&data.as_ref()[0..16]);
            let udt_amount = u128::from_le_bytes(amount_bytes);
            debug!(
                "udt_amount: {}, to_remote_amount: {}, to_local_amount: {}",
                udt_amount, self.to_remote_amount, self.to_local_amount
            );
            debug!("current_capacity: {}, remote_reserved_ckb_amount: {}, local_reserved_ckb_amount: {}",
                current_capacity, self.remote_reserved_ckb_amount, self.local_reserved_ckb_amount);
            let is_udt_amount_ok = udt_amount == self.get_total_udt_amount();
            return Ok(is_udt_amount_ok);
        } else {
            let is_complete = current_capacity == self.get_total_ckb_amount();
            Ok(is_complete)
        }
    }

    fn maybe_complete_tx_collaboration(
        &mut self,
        tx: Transaction,
        network: &ActorRef<NetworkActorMessage>,
    ) -> ProcessingChannelResult {
        let is_complete = self.is_tx_final(&tx)?;

        debug!(
            "Checking if funding transaction {:?} is complete: {}",
            &tx, is_complete
        );

        if is_complete {
            // We need to send a SendFiberMessage command here (instead of a ControlFiberChannel),
            // to guarantee that the TxComplete message immediately is sent to the network actor.
            // Otherwise, it is possible that when the network actor is processing ControlFiberChannel,
            // it receives another SendFiberMessage command, and that message (e.g. CommitmentSigned)
            // is processed first, thus breaking the order of messages.
            let commitment_tx_partial_signature = self.build_init_commitment_tx_signature()?;
            network
                .send_message(NetworkActorMessage::new_command(
                    NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                        self.get_remote_peer_id(),
                        FiberMessage::tx_complete(TxComplete {
                            channel_id: self.get_id(),
                            commitment_tx_partial_signature,
                        }),
                    )),
                ))
                .expect(ASSUME_NETWORK_ACTOR_ALIVE);
            let old_flags = match self.state {
                ChannelState::CollaboratingFundingTx(flags) => flags,
                _ => {
                    panic!(
                        "Expect to be in CollaboratingFundingTx state while running update_funding_tx, current state {:?}", &self.state,
                    );
                }
            };
            self.update_state(ChannelState::CollaboratingFundingTx(
                old_flags | CollaboratingFundingTxFlags::OUR_TX_COMPLETE_SENT,
            ));
        }
        Ok(())
    }

    fn build_init_commitment_tx_signature(&self) -> Result<PartialSignature, SigningError> {
        let key_agg_ctx = {
            let local_pubkey = self.get_local_channel_public_keys().funding_pubkey;
            let remote_pubkey = self.get_remote_channel_public_keys().funding_pubkey;
            KeyAggContext::new([remote_pubkey, local_pubkey]).expect("Valid pubkeys")
        };
        let x_only_aggregated_pubkey = key_agg_ctx.aggregated_pubkey::<Point>().serialize_xonly();
        let sign_ctx = {
            let local_nonce = self.get_local_nonce();
            let remote_nonce = self.get_remote_nonce();
            let nonces = [local_nonce, remote_nonce];
            let agg_nonce = AggNonce::sum(nonces);
            Musig2SignContext {
                key_agg_ctx,
                agg_nonce,
                seckey: self.signer.funding_key.clone(),
                secnonce: self.get_local_musig2_secnonce(),
            }
        };
        let ([to_local_output, to_remote_output], [to_local_output_data, to_remote_output_data]) =
            self.build_settlement_transaction_outputs(false);
        let version = 0u64;
        let commitment_lock_script_args = [
            &blake2b_256(x_only_aggregated_pubkey)[0..20],
            self.get_delay_epoch_as_lock_args_bytes().as_slice(),
            version.to_be_bytes().as_slice(),
        ]
        .concat();
        let message = blake2b_256(
            [
                to_local_output.as_slice(),
                to_local_output_data.as_slice(),
                to_remote_output.as_slice(),
                to_remote_output_data.as_slice(),
                commitment_lock_script_args.as_slice(),
            ]
            .concat(),
        );

        sign_ctx.sign(message.as_slice())
    }

    fn check_init_commitment_tx_signature(
        &self,
        signature: PartialSignature,
    ) -> Result<SettlementData, ProcessingChannelError> {
        let key_agg_ctx = {
            let local_pubkey = self.get_local_channel_public_keys().funding_pubkey;
            let remote_pubkey = self.get_remote_channel_public_keys().funding_pubkey;
            KeyAggContext::new([local_pubkey, remote_pubkey]).expect("Valid pubkeys")
        };
        let x_only_aggregated_pubkey = key_agg_ctx.aggregated_pubkey::<Point>().serialize_xonly();
        let local_nonce = self.get_local_nonce();
        let remote_nonce = self.get_remote_nonce();
        let nonces = [remote_nonce.clone(), local_nonce];
        let agg_nonce = AggNonce::sum(nonces);
        let verify_ctx = Musig2VerifyContext {
            key_agg_ctx: key_agg_ctx.clone(),
            agg_nonce: agg_nonce.clone(),
            pubkey: *self.get_remote_funding_pubkey(),
            pubnonce: remote_nonce,
        };
        let ([to_local_output, to_remote_output], [to_local_output_data, to_remote_output_data]) =
            self.build_settlement_transaction_outputs(true);
        let version = 0u64;
        let commitment_lock_script_args = [
            &blake2b_256(x_only_aggregated_pubkey)[0..20],
            self.get_delay_epoch_as_lock_args_bytes().as_slice(),
            version.to_be_bytes().as_slice(),
        ]
        .concat();
        let message = blake2b_256(
            [
                to_local_output.as_slice(),
                to_local_output_data.as_slice(),
                to_remote_output.as_slice(),
                to_remote_output_data.as_slice(),
                commitment_lock_script_args.as_slice(),
            ]
            .concat(),
        );

        verify_ctx.verify(signature, message.as_slice())?;

        let settlement_data = {
            let sign_ctx = Musig2SignContext {
                key_agg_ctx,
                agg_nonce,
                seckey: self.signer.funding_key.clone(),
                secnonce: self.get_local_musig2_secnonce(),
            };

            let our_signature = sign_ctx.sign(message.as_slice())?;
            let aggregated_signature = verify_ctx.aggregate_partial_signatures_for_msg(
                [signature, our_signature],
                message.as_slice(),
            )?;

            SettlementData {
                x_only_aggregated_pubkey,
                aggregated_signature,
                to_local_output,
                to_local_output_data,
                to_remote_output,
                to_remote_output_data,
            }
        };
        Ok(settlement_data)
    }

    // TODO: More checks to the funding tx.
    fn check_tx_complete_preconditions(&mut self) -> ProcessingChannelResult {
        match self.funding_tx.as_ref() {
            None => {
                return Err(ProcessingChannelError::InvalidState(
                    "Received TxComplete message without a funding transaction".to_string(),
                ));
            }
            Some(tx) => {
                debug!(
                    "Received TxComplete message, funding tx is present {:?}",
                    tx
                );
                let check = self.is_tx_final(tx);
                if !check.is_ok_and(|ok| ok) {
                    return Err(ProcessingChannelError::InvalidState(
                        "Received TxComplete message, but funding tx is not final".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn fill_in_channel_id(&mut self) {
        let local = &self.get_local_channel_public_keys().tlc_base_key;
        let remote = &self.get_remote_channel_public_keys().tlc_base_key;
        let channel_id = derive_channel_id_from_tlc_keys(local, remote);
        debug!("Channel Id changed from {:?} to {:?}", self.id, channel_id,);
        self.id = channel_id;
    }

    // Whose pubkey should go first in musig2?
    // We define a definitive order for the pubkeys in musig2 to makes it easier
    // to aggregate musig2 signatures.
    fn should_local_go_first_in_musig2(&self) -> bool {
        let local_pubkey = self.get_local_channel_public_keys().funding_pubkey;
        let remote_pubkey = self.get_remote_channel_public_keys().funding_pubkey;
        local_pubkey <= remote_pubkey
    }

    // Order some items (like pubkey and nonce) from holders and counterparty in musig2.
    fn order_things_for_musig2<T>(&self, holder: T, counterparty: T) -> [T; 2] {
        if self.should_local_go_first_in_musig2() {
            [holder, counterparty]
        } else {
            [counterparty, holder]
        }
    }

    // Should the local send tx_signatures first?
    // In order to avoid deadlock, we need to define an order for sending tx_signatures.
    // Currently the order of sending tx_signatures is defined as follows:
    // If the amount to self is less than the amount to remote, then we should send,
    // else if the amount to self is equal to the amount to remote and we have
    // smaller funding_pubkey, then we should send first. Otherwise, we should wait
    // the counterparty to send tx_signatures first.
    fn should_local_send_tx_signatures_first(&self) -> bool {
        self.to_local_amount < self.to_remote_amount
            || self.to_local_amount == self.to_remote_amount
                && self.should_local_go_first_in_musig2()
    }

    fn build_shutdown_tx(&self) -> Result<TransactionView, ProcessingChannelError> {
        let local_shutdown_info = self
            .local_shutdown_info
            .as_ref()
            .expect("local shutdown info exists");
        let remote_shutdown_info = self
            .remote_shutdown_info
            .as_ref()
            .expect("remote shutdown info exists");

        let local_shutdown_script = local_shutdown_info.close_script.clone();
        let remote_shutdown_script = remote_shutdown_info.close_script.clone();
        let local_shutdown_fee = calculate_shutdown_tx_fee(
            local_shutdown_info.fee_rate,
            &self.funding_udt_type_script,
            (
                remote_shutdown_script.clone(),
                local_shutdown_script.clone(),
            ),
        );
        let remote_shutdown_fee = calculate_shutdown_tx_fee(
            remote_shutdown_info.fee_rate,
            &self.funding_udt_type_script,
            (
                local_shutdown_script.clone(),
                remote_shutdown_script.clone(),
            ),
        );

        debug!(
            "build_shutdown_tx local_shutdown_fee: local {}, remote {}",
            local_shutdown_fee, remote_shutdown_fee
        );

        let cell_deps = get_cell_deps(vec![Contract::FundingLock], &self.funding_udt_type_script);
        let tx_builder = TransactionBuilder::default().cell_deps(cell_deps).input(
            CellInput::new_builder()
                .previous_output(self.must_get_funding_transaction_outpoint())
                .build(),
        );

        if let Some(type_script) = &self.funding_udt_type_script {
            debug!(
                "shutdown UDT local_amount: {}, remote_amount: {}",
                self.to_local_amount, self.to_remote_amount
            );

            let local_capacity: u64 = self.local_reserved_ckb_amount - local_shutdown_fee;
            debug!(
                "shutdown_tx local_capacity: {} - {} = {}",
                self.local_reserved_ckb_amount, local_shutdown_fee, local_capacity
            );
            let to_local_output = CellOutput::new_builder()
                .lock(local_shutdown_script)
                .type_(Some(type_script.clone()).pack())
                .capacity(local_capacity.pack())
                .build();
            let to_local_output_data = self.to_local_amount.to_le_bytes().pack();

            let remote_capacity: u64 = self.remote_reserved_ckb_amount - remote_shutdown_fee;
            debug!(
                "shutdown_tx remote_capacity: {} - {} = {}",
                self.remote_reserved_ckb_amount, remote_shutdown_fee, remote_capacity
            );
            let to_remote_output = CellOutput::new_builder()
                .lock(remote_shutdown_script)
                .type_(Some(type_script.clone()).pack())
                .capacity(remote_capacity.pack())
                .build();
            let to_remote_output_data = self.to_remote_amount.to_le_bytes().pack();

            let outputs = self.order_things_for_musig2(to_local_output, to_remote_output);
            let outputs_data =
                self.order_things_for_musig2(to_local_output_data, to_remote_output_data);
            let tx = tx_builder
                .set_outputs(outputs.to_vec())
                .set_outputs_data(outputs_data.to_vec())
                .build();
            Ok(tx)
        } else {
            debug!(
                "Final balance partition before shutting down: local {} (fee {}), remote {} (fee {})",
                self.to_local_amount, local_shutdown_fee,
                self.to_remote_amount, remote_shutdown_fee
            );
            let local_value =
                self.to_local_amount as u64 + self.local_reserved_ckb_amount - local_shutdown_fee;
            let remote_value = self.to_remote_amount as u64 + self.remote_reserved_ckb_amount
                - remote_shutdown_fee;
            debug!(
                "Building shutdown transaction with values: local {}, remote {}",
                local_value, remote_value
            );
            let to_local_output = CellOutput::new_builder()
                .capacity(local_value.pack())
                .lock(local_shutdown_script)
                .build();
            let to_remote_output = CellOutput::new_builder()
                .capacity(remote_value.pack())
                .lock(remote_shutdown_script)
                .build();
            let outputs = self.order_things_for_musig2(to_local_output, to_remote_output);
            let tx = tx_builder
                .set_outputs(outputs.to_vec())
                .set_outputs_data(vec![Default::default(), Default::default()])
                .build();
            Ok(tx)
        }
    }

    // The parameter `for_remote` here specifies whether we are building the commitment transaction
    // for the local party or the remote party. If `for_remote` is false, then we are building a
    // commitment transaction which can be broadcasted by ourself (with valid partial
    // signature from the other party), else we are building a commitment transaction
    // for the remote party (we build this commitment transaction
    // normally because we want to send a partial signature to remote).
    // The function returns a tuple, the first element is the commitment transaction itself,
    // and the second element is the message to be signed by the each party,
    // so as to consume the funding cell. The last element is the witnesses for the
    // commitment transaction.
    fn build_commitment_and_settlement_tx(
        &self,
        for_remote: bool,
    ) -> (TransactionView, TransactionView) {
        let commitment_tx = {
            let funding_out_point = self.must_get_funding_transaction_outpoint();
            let cell_deps =
                get_cell_deps(vec![Contract::FundingLock], &self.funding_udt_type_script);
            let (output, output_data) = self.build_commitment_transaction_output(for_remote);

            TransactionBuilder::default()
                .cell_deps(cell_deps)
                .input(
                    CellInput::new_builder()
                        .previous_output(funding_out_point.clone())
                        .build(),
                )
                .output(output)
                .output_data(output_data)
                .build()
        };

        let settlement_tx = {
            let commtimtent_out_point = OutPoint::new(commitment_tx.hash(), 0);
            let cell_deps = get_cell_deps(
                vec![Contract::CommitmentLock],
                &self.funding_udt_type_script,
            );
            let (outputs, outputs_data) = self.build_settlement_transaction_outputs(for_remote);

            TransactionBuilder::default()
                .cell_deps(cell_deps)
                .input(
                    CellInput::new_builder()
                        .previous_output(commtimtent_out_point.clone())
                        .build(),
                )
                .set_outputs(outputs.to_vec())
                .set_outputs_data(outputs_data.to_vec())
                .build()
        };

        (commitment_tx, settlement_tx)
    }

    fn build_commitment_transaction_output(&self, for_remote: bool) -> (CellOutput, Bytes) {
        let x_only_aggregated_pubkey = self.get_commitment_lock_script_xonly(for_remote);
        let version = self.get_current_commitment_number(for_remote);
        let htlcs = self.get_active_htlcs(for_remote);

        let mut commitment_lock_script_args = [
            &blake2b_256(x_only_aggregated_pubkey)[0..20],
            self.get_delay_epoch_as_lock_args_bytes().as_slice(),
            version.to_be_bytes().as_slice(),
        ]
        .concat();
        if !htlcs.is_empty() {
            commitment_lock_script_args.extend_from_slice(&blake2b_256(&htlcs)[0..20]);
        }

        let commitment_lock_script =
            get_script_by_contract(Contract::CommitmentLock, &commitment_lock_script_args);

        let commitment_tx_fee =
            calculate_commitment_tx_fee(self.commitment_fee_rate, &self.funding_udt_type_script);

        if let Some(udt_type_script) = &self.funding_udt_type_script {
            let capacity = self.local_reserved_ckb_amount + self.remote_reserved_ckb_amount
                - commitment_tx_fee;
            let output = CellOutput::new_builder()
                .lock(commitment_lock_script)
                .type_(Some(udt_type_script.clone()).pack())
                .capacity(capacity.pack())
                .build();

            let output_data = self.get_total_udt_amount().to_le_bytes().pack();
            (output, output_data)
        } else {
            let capacity = self.get_total_ckb_amount() - commitment_tx_fee;
            let output = CellOutput::new_builder()
                .lock(commitment_lock_script)
                .capacity(capacity.pack())
                .build();
            let output_data = Bytes::default();
            (output, output_data)
        }
    }

    fn get_commitment_lock_script_xonly(&self, for_remote: bool) -> [u8; 32] {
        let local_pubkey = self.get_local_channel_public_keys().funding_pubkey;
        let remote_pubkey = self.get_remote_channel_public_keys().funding_pubkey;
        let pubkeys = if for_remote {
            [local_pubkey, remote_pubkey]
        } else {
            [remote_pubkey, local_pubkey]
        };
        KeyAggContext::new(pubkeys)
            .expect("Valid pubkeys")
            .aggregated_pubkey::<Point>()
            .serialize_xonly()
    }

    fn build_settlement_transaction_outputs(
        &self,
        for_remote: bool,
    ) -> ([CellOutput; 2], [Bytes; 2]) {
        let offered_fulfilled = self.get_pending_fulfilled_tlcs_amount(for_remote, true);
        let received_fulfilled = self.get_pending_fulfilled_tlcs_amount(for_remote, false);

        let to_local_value = self.to_local_amount - offered_fulfilled + received_fulfilled;
        let to_remote_value = self.to_remote_amount - received_fulfilled + offered_fulfilled;
        let commitment_tx_fee =
            calculate_commitment_tx_fee(self.commitment_fee_rate, &self.funding_udt_type_script);

        let to_local_output_script = self.get_local_shutdown_script();
        let to_remote_output_script = self.get_remote_shutdown_script();

        // to simplify the fee calculation, we assume that the fee is double paid by both parties
        if let Some(udt_type_script) = &self.funding_udt_type_script {
            let to_local_output = CellOutput::new_builder()
                .lock(to_local_output_script)
                .type_(Some(udt_type_script.clone()).pack())
                .capacity((self.local_reserved_ckb_amount - commitment_tx_fee).pack())
                .build();
            let to_local_output_data = to_local_value.to_le_bytes().pack();

            let to_remote_output = CellOutput::new_builder()
                .lock(to_remote_output_script)
                .type_(Some(udt_type_script.clone()).pack())
                .capacity((self.remote_reserved_ckb_amount - commitment_tx_fee).pack())
                .build();
            let to_remote_output_data = to_remote_value.to_le_bytes().pack();
            if for_remote {
                (
                    [to_local_output, to_remote_output],
                    [to_local_output_data, to_remote_output_data],
                )
            } else {
                (
                    [to_remote_output, to_local_output],
                    [to_remote_output_data, to_local_output_data],
                )
            }
        } else {
            let to_local_output = CellOutput::new_builder()
                .lock(to_local_output_script)
                .capacity(
                    (to_local_value as u64 + self.local_reserved_ckb_amount - commitment_tx_fee)
                        .pack(),
                )
                .build();
            let to_local_output_data = Bytes::default();

            let to_remote_output = CellOutput::new_builder()
                .lock(to_remote_output_script)
                .capacity(
                    (to_remote_value as u64 + self.remote_reserved_ckb_amount - commitment_tx_fee)
                        .pack(),
                )
                .build();
            let to_remote_output_data = Bytes::default();
            if for_remote {
                (
                    [to_local_output, to_remote_output],
                    [to_local_output_data, to_remote_output_data],
                )
            } else {
                (
                    [to_remote_output, to_local_output],
                    [to_remote_output_data, to_local_output_data],
                )
            }
        }
    }

    pub fn build_and_verify_commitment_tx(
        &self,
        funding_tx_partial_signature: PartialSignature,
        commitment_tx_partial_signature: PartialSignature,
    ) -> Result<PartiallySignedCommitmentTransaction, ProcessingChannelError> {
        let (commitment_tx, settlement_tx) = self.build_commitment_and_settlement_tx(false);

        let verify_ctx = Musig2VerifyContext::from(self);
        verify_ctx.verify(
            funding_tx_partial_signature,
            commitment_tx.hash().as_slice(),
        )?;

        let verify_ctx = Musig2VerifyContext::from((self, false));
        let to_local_output = settlement_tx
            .outputs()
            .get(0)
            .expect("get output 0 of settlement tx");
        let to_local_output_data = settlement_tx
            .outputs_data()
            .get(0)
            .expect("get output 0 data of settlement tx");
        let to_remote_output = settlement_tx
            .outputs()
            .get(1)
            .expect("get output 1 of settlement tx");
        let to_remote_output_data = settlement_tx
            .outputs_data()
            .get(1)
            .expect("get output 1 data of settlement tx");
        let args = commitment_tx
            .outputs()
            .get(0)
            .expect("get output 0 of commitment tx")
            .lock()
            .args()
            .raw_data();
        let message = blake2b_256(
            [
                to_local_output.as_slice(),
                to_local_output_data.as_slice(),
                to_remote_output.as_slice(),
                to_remote_output_data.as_slice(),
                &args[0..36],
            ]
            .concat(),
        );
        verify_ctx.verify(commitment_tx_partial_signature, message.as_slice())?;

        Ok(PartiallySignedCommitmentTransaction {
            version: self.get_current_commitment_number(false),
            commitment_tx,
            settlement_tx,
            funding_tx_partial_signature,
            commitment_tx_partial_signature,
        })
    }

    fn build_and_sign_commitment_tx(
        &self,
    ) -> Result<(PartialSignature, PartialSignature), ProcessingChannelError> {
        let (commitment_tx, settlement_tx) = self.build_commitment_and_settlement_tx(true);

        let sign_ctx = Musig2SignContext::from(self);
        let funding_tx_partial_signature = sign_ctx.sign(commitment_tx.hash().as_slice())?;

        let sign_ctx = Musig2SignContext::from((self, true));
        let to_local_output = settlement_tx
            .outputs()
            .get(0)
            .expect("get output 0 of settlement tx");
        let to_local_output_data = settlement_tx
            .outputs_data()
            .get(0)
            .expect("get output 0 data of settlement tx");
        let to_remote_output = settlement_tx
            .outputs()
            .get(1)
            .expect("get output 1 of settlement tx");
        let to_remote_output_data = settlement_tx
            .outputs_data()
            .get(1)
            .expect("get output 1 data of settlement tx");
        let args = commitment_tx
            .outputs()
            .get(0)
            .expect("get output 0 of commitment tx")
            .lock()
            .args()
            .raw_data();
        let message = blake2b_256(
            [
                to_local_output.as_slice(),
                to_local_output_data.as_slice(),
                to_remote_output.as_slice(),
                to_remote_output_data.as_slice(),
                &args[0..36],
            ]
            .concat(),
        );

        let commitment_tx_partial_signature = sign_ctx.sign(message.as_slice())?;

        Ok((
            funding_tx_partial_signature,
            commitment_tx_partial_signature,
        ))
    }

    /// Verify the partial signature from the peer and create a complete transaction
    /// with valid witnesses.
    fn verify_and_complete_tx(
        &self,
        funding_tx_partial_signature: PartialSignature,
        commitment_tx_partial_signature: PartialSignature,
    ) -> Result<(TransactionView, SettlementData), ProcessingChannelError> {
        let tx = self.build_and_verify_commitment_tx(
            funding_tx_partial_signature,
            commitment_tx_partial_signature,
        )?;
        self.complete_partially_signed_tx(&tx)
    }

    fn get_delay_epoch_as_lock_args_bytes(&self) -> [u8; 8] {
        let since = Since::new(
            SinceType::EpochNumberWithFraction,
            self.commitment_delay_epoch,
            true,
        );
        since.value().to_le_bytes()
    }
}

pub trait ChannelActorStateStore {
    fn get_channel_actor_state(&self, id: &Hash256) -> Option<ChannelActorState>;
    fn insert_channel_actor_state(&self, state: ChannelActorState);
    fn delete_channel_actor_state(&self, id: &Hash256);
    fn get_channel_ids_by_peer(&self, peer_id: &PeerId) -> Vec<Hash256>;
    fn get_active_channel_ids_by_peer(&self, peer_id: &PeerId) -> Vec<Hash256> {
        self.get_channel_ids_by_peer(peer_id)
            .into_iter()
            .filter(
                |id| matches!(self.get_channel_actor_state(id), Some(state) if !state.is_closed()),
            )
            .collect()
    }
    fn get_channel_states(&self, peer_id: Option<PeerId>) -> Vec<(PeerId, Hash256, ChannelState)>;
    fn get_active_channel_states(
        &self,
        peer_id: Option<PeerId>,
    ) -> Vec<(PeerId, Hash256, ChannelState)> {
        self.get_channel_states(peer_id)
            .into_iter()
            .filter(|(_, _, state)| !state.is_closed())
            .collect()
    }
    fn get_channel_state_by_outpoint(&self, id: &OutPoint) -> Option<ChannelActorState>;
}

/// A wrapper on CommitmentTransaction that has a partial signature along with
/// the ckb transaction.
#[derive(Clone, Debug)]
pub struct PartiallySignedCommitmentTransaction {
    // The version number of the commitment transaction.
    pub version: u64,
    // The commitment transaction.
    pub commitment_tx: TransactionView,
    // The settlement transaction.
    pub settlement_tx: TransactionView,
    // The partial signature to unlock the funding transaction.
    pub funding_tx_partial_signature: PartialSignature,
    // The partial signature to unlock the commitment transaction.
    pub commitment_tx_partial_signature: PartialSignature,
}

pub fn create_witness_for_funding_cell(
    lock_key_xonly: [u8; 32],
    signature: CompactSignature,
) -> [u8; FUNDING_CELL_WITNESS_LEN] {
    let mut witness = Vec::with_capacity(FUNDING_CELL_WITNESS_LEN);

    // for xudt compatibility issue,
    // refer to: https://github.com/nervosnetwork/fiber-scripts/pull/5
    let empty_witness_args = [16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0];
    witness.extend_from_slice(&empty_witness_args);
    witness.extend_from_slice(lock_key_xonly.as_slice());
    witness.extend_from_slice(signature.serialize().as_slice());
    witness
        .try_into()
        .expect("Witness length should be correct")
}

pub fn create_witness_for_commitment_cell(
    lock_key_xonly: [u8; 32],
    signature: CompactSignature,
) -> [u8; COMMITMENT_CELL_WITNESS_LEN] {
    let mut witness = Vec::with_capacity(COMMITMENT_CELL_WITNESS_LEN);
    // for xudt compatibility issue,
    // refer to: https://github.com/nervosnetwork/fiber-scripts/pull/5
    let empty_witness_args = [16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0];
    witness.extend_from_slice(&empty_witness_args);
    witness.extend_from_slice(&[0xFE]);
    witness.extend_from_slice(lock_key_xonly.as_slice());
    witness.extend_from_slice(signature.serialize().as_slice());
    witness
        .try_into()
        .expect("Witness length should be correct")
}

pub struct Musig2VerifyContext {
    pub key_agg_ctx: KeyAggContext,
    pub agg_nonce: AggNonce,
    pub pubkey: Pubkey,
    pub pubnonce: PubNonce,
}

impl Musig2VerifyContext {
    pub fn verify(&self, signature: PartialSignature, message: &[u8]) -> Result<(), VerifyError> {
        verify_partial(
            &self.key_agg_ctx,
            signature,
            &self.agg_nonce,
            self.pubkey,
            &self.pubnonce,
            message,
        )
    }

    pub fn aggregate_partial_signatures_for_msg(
        &self,
        partial_signatures: [PartialSignature; 2],
        message: &[u8],
    ) -> Result<CompactSignature, VerifyError> {
        aggregate_partial_signatures(
            &self.key_agg_ctx,
            &self.agg_nonce,
            partial_signatures,
            message,
        )
    }
}

#[derive(Clone)]
pub struct Musig2SignContext {
    key_agg_ctx: KeyAggContext,
    agg_nonce: AggNonce,
    seckey: Privkey,
    secnonce: SecNonce,
}

impl Musig2SignContext {
    pub fn sign(self, message: &[u8]) -> Result<PartialSignature, SigningError> {
        sign_partial(
            &self.key_agg_ctx,
            self.seckey,
            self.secnonce.clone(),
            &self.agg_nonce,
            message,
        )
    }
}

/// One counterparty's public keys which do not change over the life of a channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelBasePublicKeys {
    /// The public key which is used to sign all commitment transactions, as it appears in the
    /// on-chain channel lock-in 2-of-2 multisig output.
    pub funding_pubkey: Pubkey,
    /// The base point which is used (with derive_public_key) to derive a per-commitment public key
    /// which is used to encumber HTLC-in-flight outputs.
    pub tlc_base_key: Pubkey,
}

impl From<&OpenChannel> for ChannelBasePublicKeys {
    fn from(value: &OpenChannel) -> Self {
        ChannelBasePublicKeys {
            funding_pubkey: value.funding_pubkey,
            tlc_base_key: value.tlc_basepoint,
        }
    }
}

impl From<&AcceptChannel> for ChannelBasePublicKeys {
    fn from(value: &AcceptChannel) -> Self {
        ChannelBasePublicKeys {
            funding_pubkey: value.funding_pubkey,
            tlc_base_key: value.tlc_basepoint,
        }
    }
}

type ShortHash = [u8; 20];

pub fn get_tweak_by_commitment_point(commitment_point: &Pubkey) -> [u8; 32] {
    let mut hasher = new_blake2b();
    hasher.update(&commitment_point.serialize());
    let mut result = [0u8; 32];
    hasher.finalize(&mut result);
    result
}

pub(crate) fn derive_private_key(secret: &Privkey, commitment_point: &Pubkey) -> Privkey {
    secret.tweak(get_tweak_by_commitment_point(commitment_point))
}

fn derive_public_key(base_key: &Pubkey, commitment_point: &Pubkey) -> Pubkey {
    base_key.tweak(get_tweak_by_commitment_point(commitment_point))
}

pub fn derive_payment_pubkey(base_key: &Pubkey, commitment_point: &Pubkey) -> Pubkey {
    derive_public_key(base_key, commitment_point)
}

pub fn derive_delayed_payment_pubkey(base_key: &Pubkey, commitment_point: &Pubkey) -> Pubkey {
    derive_public_key(base_key, commitment_point)
}

pub fn derive_tlc_pubkey(base_key: &Pubkey, commitment_point: &Pubkey) -> Pubkey {
    derive_public_key(base_key, commitment_point)
}

/// A simple implementation of [`WriteableEcdsaChannelSigner`] that just keeps the private keys in memory.
///
/// This implementation performs no policy checks and is insufficient by itself as
/// a secure external signer.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct InMemorySigner {
    /// Holder secret key in the 2-of-2 multisig script of a channel. This key also backs the
    /// holder's anchor output in a commitment transaction, if one is present.
    pub funding_key: Privkey,
    /// Holder HTLC secret key used in commitment transaction HTLC outputs.
    pub tlc_base_key: Privkey,
    /// SecNonce used to generate valid signature in musig.
    // TODO: use rust's ownership to make sure musig_nonce is used once.
    pub musig2_base_nonce: Privkey,
    /// Seed to derive above keys (per commitment).
    pub commitment_seed: [u8; 32],
}

impl InMemorySigner {
    pub fn generate_from_seed(params: &[u8]) -> Self {
        let seed = ckb_hash::blake2b_256(params);

        let commitment_seed = {
            let mut hasher = new_blake2b();
            hasher.update(&seed);
            hasher.update(&b"commitment seed"[..]);
            let mut result = [0u8; 32];
            hasher.finalize(&mut result);
            result
        };

        let key_derive = |seed: &[u8], info: &[u8]| {
            let result = blake2b_hash_with_salt(seed, info);
            Privkey::from_slice(&result)
        };

        let funding_key = key_derive(&seed, b"funding key");
        let tlc_base_key = key_derive(funding_key.as_ref(), b"HTLC base key");
        let musig2_base_nonce = key_derive(tlc_base_key.as_ref(), b"musig nocne");

        Self {
            funding_key,
            tlc_base_key,
            musig2_base_nonce,
            commitment_seed,
        }
    }

    fn get_base_public_keys(&self) -> ChannelBasePublicKeys {
        ChannelBasePublicKeys {
            funding_pubkey: self.funding_key.pubkey(),
            tlc_base_key: self.tlc_base_key.pubkey(),
        }
    }

    pub fn get_commitment_point(&self, commitment_number: u64) -> Pubkey {
        get_commitment_point(&self.commitment_seed, commitment_number)
    }

    pub fn get_commitment_secret(&self, commitment_number: u64) -> [u8; 32] {
        get_commitment_secret(&self.commitment_seed, commitment_number)
    }

    pub fn derive_tlc_key(&self, new_commitment_number: u64) -> Privkey {
        let per_commitment_point = self.get_commitment_point(new_commitment_number);
        derive_private_key(&self.tlc_base_key, &per_commitment_point)
    }

    // TODO: Verify that this is a secure way to derive the nonce.
    pub fn derive_musig2_nonce(&self, commitment_number: u64) -> SecNonce {
        let commitment_point = self.get_commitment_point(commitment_number);
        let seckey = derive_private_key(&self.musig2_base_nonce, &commitment_point);
        SecNonce::build(seckey.as_ref()).build()
    }
}
