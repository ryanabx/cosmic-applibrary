use std::os::{
    fd::{FromRawFd, RawFd},
    unix::net::UnixStream,
};

use cctk::{
    sctk::{
        activation::{ActivationHandler, ActivationState, RequestData, RequestDataExt},
        reexports::{calloop, calloop_wayland_source::WaylandSource},
        registry::{ProvidesRegistryState, RegistryState},
        seat::{SeatHandler, SeatState},
    },
    wayland_client::{
        globals::registry_queue_init,
        protocol::{wl_seat::WlSeat, wl_surface::WlSurface},
        Connection, QueueHandle,
    },
};
use cosmic::iced::{self, subscription};
use futures::{
    channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
    SinkExt, StreamExt,
};
use once_cell::sync::Lazy;
use tokio::sync::Mutex;

struct AppData {
    exit: bool,
    tx: UnboundedSender<WaylandUpdate>,
    conn: Connection,
    queue_handle: QueueHandle<Self>,
    registry_state: RegistryState,
    seat_state: SeatState,
    activation_state: Option<ActivationState>,
}

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    cctk::sctk::registry_handlers!();
}

impl SeatHandler for AppData {
    fn seat_state(&mut self) -> &mut cctk::sctk::seat::SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        _: cctk::sctk::seat::Capability,
    ) {
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        _: cctk::sctk::seat::Capability,
    ) {
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

struct ExecRequestData {
    data: RequestData,
    exec: String,
    gpu_idx: Option<usize>,
}

impl RequestDataExt for ExecRequestData {
    fn app_id(&self) -> Option<&str> {
        self.data.app_id()
    }

    fn seat_and_serial(&self) -> Option<(&WlSeat, u32)> {
        self.data.seat_and_serial()
    }

    fn surface(&self) -> Option<&WlSurface> {
        self.data.surface()
    }
}

impl ActivationHandler for AppData {
    type RequestData = ExecRequestData;
    fn new_token(&mut self, token: String, data: &ExecRequestData) {
        let _ = self.tx.unbounded_send(WaylandUpdate::ActivationToken {
            token: Some(token),
            exec: data.exec.clone(),
            gpu_idx: data.gpu_idx,
        });
    }
}

fn wayland_handler(
    tx: UnboundedSender<WaylandUpdate>,
    rx: calloop::channel::Channel<WaylandRequest>,
) {
    let socket = std::env::var("X_PRIVILEGED_WAYLAND_SOCKET")
        .ok()
        .and_then(|fd| {
            fd.parse::<RawFd>()
                .ok()
                .map(|fd| unsafe { UnixStream::from_raw_fd(fd) })
        });

    let conn = if let Some(socket) = socket {
        Connection::from_socket(socket).unwrap()
    } else {
        Connection::connect_to_env().unwrap()
    };
    let (globals, event_queue) = registry_queue_init(&conn).unwrap();

    let mut event_loop = calloop::EventLoop::<AppData>::try_new().unwrap();
    let qh = event_queue.handle();
    let wayland_source = WaylandSource::new(conn.clone(), event_queue);
    let handle = event_loop.handle();
    wayland_source
        .insert(handle.clone())
        .expect("Failed to insert wayland source.");

    if handle
        .insert_source(rx, |event, _, state| match event {
            calloop::channel::Event::Msg(req) => match req {
                WaylandRequest::TokenRequest {
                    app_id,
                    exec,
                    gpu_idx,
                } => {
                    if let Some(activation_state) = state.activation_state.as_ref() {
                        activation_state.request_token_with_data(
                            &state.queue_handle,
                            ExecRequestData {
                                data: RequestData {
                                    app_id: Some(app_id),
                                    seat_and_serial: state
                                        .seat_state
                                        .seats()
                                        .next()
                                        .map(|seat| (seat, 0)),
                                    surface: None,
                                },
                                exec,
                                gpu_idx,
                            },
                        );
                    } else {
                        let _ = state.tx.unbounded_send(WaylandUpdate::ActivationToken {
                            token: None,
                            exec,
                            gpu_idx,
                        });
                    }
                }
            },
            calloop::channel::Event::Closed => {
                state.exit = true;
            }
        })
        .is_err()
    {
        return;
    }

    let registry_state = RegistryState::new(&globals);

    let mut app_data = AppData {
        exit: false,
        tx,
        conn,
        queue_handle: qh.clone(),
        registry_state,
        seat_state: SeatState::new(&globals, &qh),
        activation_state: ActivationState::bind::<AppData>(&globals, &qh).ok(),
    };

    loop {
        if app_data.exit {
            break;
        }
        event_loop.dispatch(None, &mut app_data).unwrap();
    }
}

#[derive(Clone, Debug)]
pub enum WaylandRequest {
    TokenRequest {
        app_id: String,
        exec: String,
        gpu_idx: Option<usize>,
    },
}

#[derive(Clone, Debug)]
pub enum WaylandUpdate {
    Init(calloop::channel::Sender<WaylandRequest>),
    ActivationToken {
        token: Option<String>,
        exec: String,
        gpu_idx: Option<usize>,
    },
    Finished,
}

pub static WAYLAND_RX: Lazy<Mutex<Option<UnboundedReceiver<WaylandUpdate>>>> =
    Lazy::new(|| Mutex::new(None));

pub fn wayland_subscription() -> iced::Subscription<WaylandUpdate> {
    subscription::channel(
        std::any::TypeId::of::<WaylandUpdate>(),
        50,
        move |mut output| async move {
            let mut state = State::Waiting;

            loop {
                state = start_listening(state, &mut output).await;
            }
        },
    )
}

pub enum State {
    Waiting,
    Finished,
}

async fn start_listening(
    state: State,
    output: &mut futures::channel::mpsc::Sender<WaylandUpdate>,
) -> State {
    match state {
        State::Waiting => {
            let mut guard = WAYLAND_RX.lock().await;
            let rx = {
                if guard.is_none() {
                    let (calloop_tx, calloop_rx) = calloop::channel::channel();
                    let (toplevel_tx, toplevel_rx) = unbounded();
                    let _ = std::thread::spawn(move || {
                        wayland_handler(toplevel_tx, calloop_rx);
                    });
                    *guard = Some(toplevel_rx);
                    _ = output.send(WaylandUpdate::Init(calloop_tx)).await;
                }
                guard.as_mut().unwrap()
            };
            match rx.next().await {
                Some(u) => {
                    _ = output.send(u).await;
                    State::Waiting
                }
                None => {
                    _ = output.send(WaylandUpdate::Finished).await;
                    log::error!("Wayland handler thread died");
                    State::Finished
                }
            }
        }
        State::Finished => iced::futures::future::pending().await,
    }
}

cctk::sctk::delegate_seat!(AppData);
cctk::sctk::delegate_registry!(AppData);
cctk::sctk::delegate_activation!(AppData, ExecRequestData);
