#![doc = include_str!("./networking.md")]

use ggrs::{NetworkStats, P2PSession, PlayerHandle};
use jumpy_core::input::PlayerControl;
use rand::Rng;

use crate::{
    networking::debug::{NetworkDebugMessage, NETWORK_DEBUG_CHANNEL},
    prelude::*,
};

pub mod certs;
pub mod debug;
pub mod lan;
pub mod online;
pub mod proto;

/// The muliplier for the [`jumpy_core::FPS`] that will be used when playing an online match.
///
/// Lowering the frame rate a little for online matches reduces bandwidth and may help overall
/// gameplay. This may not be necessary once we improve network performance.
pub const NETWORK_FRAME_RATE_FACTOR: f32 = 0.9;

/// Number of frames client may predict beyond confirmed frame before freezing and waiting
/// for inputs from other players.
pub const NETWORK_MAX_PREDICTION_WINDOW: usize = 10;

/// The [`ggrs::Config`] implementation used by Jumpy.
#[derive(Debug)]
pub struct GgrsConfig;
impl ggrs::Config for GgrsConfig {
    type Input = proto::DensePlayerControl;
    type State = bones::World;
    /// Addresses are the same as the player handle for our custom socket.
    type Address = usize;
}

/// The network endpoint used for all QUIC network communications.
pub static NETWORK_ENDPOINT: Lazy<quinn::Endpoint> = Lazy::new(|| {
    // Generate certificate
    let (cert, key) = certs::generate_self_signed_cert().unwrap();

    let mut transport_config = quinn::TransportConfig::default();
    transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(5)));

    let mut server_config = quinn::ServerConfig::with_single_cert([cert].to_vec(), key).unwrap();
    server_config.transport = Arc::new(transport_config);

    // Open Socket and create endpoint
    let port = rand::thread_rng().gen_range(10000..=11000); // Bind a random port
    info!(port, "Started network endpoint");
    let socket = std::net::UdpSocket::bind(("0.0.0.0", port)).unwrap();

    let client_config = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(certs::SkipServerVerification::new())
        .with_no_client_auth();
    let client_config = quinn::ClientConfig::new(Arc::new(client_config));

    let mut endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        socket,
        Arc::new(quinn_runtime_bevy::BevyIoTaskPoolExecutor),
    )
    .unwrap();

    endpoint.set_default_client_config(client_config);

    endpoint
});

/// Resource containing the [`NetworkSocket`] implementation while there is a connection to a
/// network game.
///
/// This is inserted into the world after a match has been established by a network matchmaker.
#[derive(Resource, Deref, DerefMut)]
pub struct NetworkMatchSocket(pub Box<dyn NetworkSocket>);

/// A type-erased [`ggrs::NonBlockingSocket`][crate::external::ggrs::NonBlockingSocket]
/// implementation.
#[derive(Deref, DerefMut)]
pub struct BoxedNonBlockingSocket(Box<dyn ggrs::NonBlockingSocket<usize> + 'static>);

impl ggrs::NonBlockingSocket<usize> for BoxedNonBlockingSocket {
    fn send_to(&mut self, msg: &ggrs::Message, addr: &usize) {
        self.0.send_to(msg, addr)
    }

    fn receive_all_messages(&mut self) -> Vec<(usize, ggrs::Message)> {
        self.0.receive_all_messages()
    }
}

/// Trait that must be implemented by socket connections establish by matchmakers.
///
/// The [`NetworkMatchSocket`] resource will contain an instance of this trait and will be used by
/// the game to send network messages after a match has been established.
pub trait NetworkSocket: Sync + Send {
    /// Get a GGRS socket from this network socket.
    fn ggrs_socket(&self) -> BoxedNonBlockingSocket;
    /// Send a reliable message to the given [`SocketTarget`].
    fn send_reliable(&self, target: SocketTarget, message: &[u8]);
    /// Receive reliable messages from other players. The `usize` is the index of the player that
    /// sent the message.
    fn recv_reliable(&self) -> Vec<(usize, Vec<u8>)>;
    /// Close the connection.
    fn close(&self);
    /// Get the player index of the local player.
    fn player_idx(&self) -> usize;
    /// Return, for every player index, whether the player is a local player.
    fn player_is_local(&self) -> [bool; MAX_PLAYERS];
    /// Get the player count for this network match.
    fn player_count(&self) -> usize;
}

/// The destination for a reliable network message.
pub enum SocketTarget {
    /// Send to a specific player.
    Player(usize),
    /// Broadcast to all players.
    All,
}

/// [`SessionRunner`] implementation that uses [`ggrs`][crate::external::ggrs] for network play.
///
/// This is where the whole `ggrs` integration is implemented.
pub struct GgrsSessionRunner {
    /// The last player input we detected.
    pub last_player_input: PlayerControl,
    /// The core game session.
    pub core: CoreSession,
    /// The GGRS peer-to-peer session.
    pub session: P2PSession<GgrsConfig>,
    /// Array containing a flag indicating, for each player, whether they are a local player.
    pub player_is_local: [bool; MAX_PLAYERS],
    /// The frame time delta.
    pub delta: f32,
    /// The frame time accumulator, used to produce a fixed refresh rate.
    pub accumulator: f32,
}

/// The info required to create a [`GgrsSessionRunner`].
pub struct GgrsSessionRunnerInfo {
    /// The GGRS socket implementation to use.
    pub socket: BoxedNonBlockingSocket,
    /// The list of local players.
    pub player_is_local: [bool; MAX_PLAYERS],
    /// the player count.
    pub player_count: usize,
}

impl GgrsSessionRunner {
    /// Create a new sessino runner.
    pub fn new(mut core: CoreSession, info: GgrsSessionRunnerInfo) -> Self
    where
        Self: Sized,
    {
        core.time_step = 1.0 / (jumpy_core::FPS * NETWORK_FRAME_RATE_FACTOR);
        let mut builder = ggrs::SessionBuilder::new()
            .with_num_players(info.player_count)
            .with_max_prediction_window(NETWORK_MAX_PREDICTION_WINDOW)
            .with_input_delay(1)
            .with_fps((jumpy_core::FPS * NETWORK_FRAME_RATE_FACTOR) as usize)
            .unwrap();

        for i in 0..info.player_count {
            if info.player_is_local[i] {
                builder = builder.add_player(ggrs::PlayerType::Local, i).unwrap();
            } else {
                builder = builder.add_player(ggrs::PlayerType::Remote(i), i).unwrap();
            }
        }

        let session = builder.start_p2p_session(info.socket).unwrap();

        Self {
            last_player_input: PlayerControl::default(),
            core,
            session,
            player_is_local: info.player_is_local,
            accumulator: default(),
            delta: default(),
        }
    }
}

/// Get a [`proto::DensePlayerControl`] from a normal [`PlayerControl`].
fn get_dense_input(control: &PlayerControl) -> proto::DensePlayerControl {
    let mut dense_control = proto::DensePlayerControl::default();
    dense_control.set_jump_pressed(control.jump_just_pressed);
    dense_control.set_grab_pressed(control.grab_pressed);
    dense_control.set_slide_pressed(control.slide_pressed);
    dense_control.set_shoot_pressed(control.shoot_pressed);
    dense_control.set_move_direction(proto::DenseMoveDirection(control.move_direction));
    dense_control
}

impl crate::session::SessionRunner for GgrsSessionRunner {
    fn core_session(&mut self) -> &mut CoreSession {
        &mut self.core
    }

    fn restart(&mut self) {
        self.core.restart()
    }

    fn set_player_input(&mut self, player_idx: usize, control: PlayerControl) {
        if !self.player_is_local[player_idx] {
            return;
        }
        self.last_player_input = control;
    }

    fn advance(&mut self, bevy_world: &mut World) -> Result<(), SessionError> {
        const STEP: f32 = 1.0 / (jumpy_core::FPS * NETWORK_FRAME_RATE_FACTOR);
        let delta = self.delta;
        let local_player_idx = self.network_player_idx().unwrap();

        self.accumulator += delta;

        let mut skip_frames = 0;

        // Current frame before we start network update loop
        let current_frame_original = self.session.current_frame();
        for event in self.session.events() {
            match event {
                ggrs::GGRSEvent::Synchronizing { addr, total, count } => {
                    info!(player=%addr, %total, progress=%count, "Syncing network player");
                }
                ggrs::GGRSEvent::Synchronized { addr } => {
                    info!(player=%addr, "Syncrhonized network client");
                }
                ggrs::GGRSEvent::Disconnected { .. } => return Err(SessionError::Disconnected),
                ggrs::GGRSEvent::NetworkInterrupted { addr, .. } => {
                    info!(player=%addr, "Network player interrupted");
                }
                ggrs::GGRSEvent::NetworkResumed { addr } => {
                    info!(player=%addr, "Network player re-connected");
                }
                ggrs::GGRSEvent::WaitRecommendation {
                    skip_frames: skip_count,
                } => {
                    info!(
                        "Skipping {skip_count} frames to give network players a chance to catch up"
                    );
                    skip_frames = skip_count;
                    NETWORK_DEBUG_CHANNEL
                        .sender
                        .try_send(NetworkDebugMessage::SkipFrame {
                            frame: current_frame_original,
                            count: skip_count,
                        })
                        .unwrap();
                }
                ggrs::GGRSEvent::DesyncDetected {
                    frame,
                    local_checksum,
                    remote_checksum,
                    addr,
                } => {
                    error!(%frame, %local_checksum, %remote_checksum, player=%addr, "Network de-sync detected");
                }
            }
        }

        loop {
            self.session
                .add_local_input(local_player_idx, get_dense_input(&self.last_player_input))
                .unwrap();
            if self.accumulator >= STEP {
                self.accumulator -= STEP;

                let current_frame = self.session.current_frame();
                let confirmed_frame = self.session.confirmed_frame();
                NETWORK_DEBUG_CHANNEL
                    .sender
                    .try_send(NetworkDebugMessage::FrameUpdate {
                        current: current_frame,
                        last_confirmed: confirmed_frame,
                    })
                    .unwrap();

                if skip_frames > 0 {
                    skip_frames = skip_frames.saturating_sub(1);
                    continue;
                }

                match self.session.advance_frame() {
                    Ok(requests) => {
                        for request in requests {
                            match request {
                                ggrs::GGRSRequest::SaveGameState { cell, frame } => {
                                    cell.save(frame, Some(self.core.world.clone()), None)
                                }
                                ggrs::GGRSRequest::LoadGameState { cell, .. } => {
                                    let world = cell.load().unwrap_or_default();
                                    self.core.world = world;
                                }
                                ggrs::GGRSRequest::AdvanceFrame {
                                    inputs: network_inputs,
                                } => {
                                    self.core.update_input(|inputs| {
                                        for (player_idx, (input, _status)) in
                                            network_inputs.into_iter().enumerate()
                                        {
                                            let control = &mut inputs.players[player_idx].control;

                                            let jump_pressed = input.jump_pressed();
                                            control.jump_just_pressed =
                                                jump_pressed && !control.jump_pressed;
                                            control.jump_pressed = jump_pressed;

                                            let grab_pressed = input.grab_pressed();
                                            control.grab_just_pressed =
                                                grab_pressed && !control.grab_pressed;
                                            control.grab_pressed = grab_pressed;

                                            let shoot_pressed = input.shoot_pressed();
                                            control.shoot_just_pressed =
                                                shoot_pressed && !control.shoot_pressed;
                                            control.shoot_pressed = shoot_pressed;

                                            let was_moving =
                                                control.move_direction.length_squared()
                                                    > f32::MIN_POSITIVE;
                                            control.move_direction = input.move_direction().0;
                                            let is_moving = control.move_direction.length_squared()
                                                > f32::MIN_POSITIVE;
                                            control.just_moved = !was_moving && is_moving;
                                        }
                                    });
                                    self.core.advance(bevy_world);
                                }
                            }
                        }
                    }
                    Err(e) => match e {
                        ggrs::GGRSError::NotSynchronized => {
                            debug!("Waiting for network clients to sync")
                        }
                        ggrs::GGRSError::PredictionThreshold => {
                            warn!("Freezing game while waiting for network to catch-up.");
                            NETWORK_DEBUG_CHANNEL
                                .sender
                                .try_send(NetworkDebugMessage::FrameFroze {
                                    frame: self.session.current_frame(),
                                })
                                .unwrap();
                        }
                        e => error!("Network protocol error: {e}"),
                    },
                }
            } else {
                break;
            }
        }

        // Fetch GGRS network stats of remote players and send to net debug tool
        let mut network_stats: Vec<(PlayerHandle, NetworkStats)> = vec![];
        for handle in self.session.remote_player_handles().iter() {
            if let Ok(stats) = self.session.network_stats(*handle) {
                network_stats.push((*handle, stats));
            }
        }
        if !network_stats.is_empty() {
            NETWORK_DEBUG_CHANNEL
                .sender
                .try_send(NetworkDebugMessage::NetworkStats { network_stats })
                .unwrap();
        }

        Ok(())
    }

    fn run_criteria(&mut self, time: &Time) -> ShouldRun {
        self.delta = time.delta_seconds();
        ShouldRun::Yes
    }

    fn network_player_idx(&mut self) -> Option<usize> {
        // We are the first local player
        for i in 0..MAX_PLAYERS {
            if self.player_is_local[i] {
                return Some(i);
            }
        }
        unreachable!();
    }
}
