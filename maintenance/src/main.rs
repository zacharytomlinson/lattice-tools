//! Loopback-only maintenance window. Serial ownership and transfers stay in a
//! blocking worker; browser progress never polls the radio companion.
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response, Sse,
        sse::{Event as SseEvent, KeepAlive},
    },
    routing::{get, post},
};
use lattice_peer::{
    Client, Event, Packet,
    update::{MAX_PACKAGE_BYTES, Package, UploadEvent},
};
use serde::Serialize;
use std::{
    convert::Infallible,
    io::{self, Read},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;
use tokio_stream::{StreamExt, wrappers::WatchStream};

#[derive(Clone, Serialize)]
struct Contact {
    address: String,
    via: String,
    responded: bool,
}
#[derive(Clone, Serialize)]
struct View {
    device: String,
    local: Option<String>,
    channel: Option<u8>,
    contacts: Vec<Contact>,
    incomplete: Vec<String>,
    stage: String,
    message: String,
    target: Option<String>,
    release: Option<String>,
    accepted: u32,
    committed: u32,
    total: u32,
    prepared: u16,
    changes: u64,
    lost_changes: u64,
    busy: bool,
}
struct Maintenance {
    device: String,
    authority: String,
    token: String,
    view: Mutex<View>,
    events: watch::Sender<View>,
    busy: AtomicBool,
}
impl Maintenance {
    fn publish(&self, change: impl FnOnce(&mut View)) {
        let mut view = self.view.lock().unwrap();
        change(&mut view);
        self.events.send_replace(view.clone());
    }
    fn begin(self: &Arc<Self>, stage: &str) -> Result<Job, ApiError> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                ApiError(
                    StatusCode::CONFLICT,
                    "another operation is in progress".into(),
                )
            })?;
        self.publish(|v| {
            v.stage = stage.into();
            v.busy = true;
            v.message.clear();
        });
        Ok(Job(self.clone()))
    }
    fn fail(&self, error: impl std::fmt::Display) {
        self.publish(|v| {
            v.stage = "error".into();
            v.message = error.to_string();
        });
    }
}
struct Job(Arc<Maintenance>);
impl Drop for Job {
    fn drop(&mut self) {
        self.0.publish(|v| v.busy = false);
        self.0.busy.store(false, Ordering::Release);
    }
}
struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let device = args
        .next()
        .ok_or("usage: lattice-maintenance SERIAL_DEVICE [LOCAL_PORT]")?;
    let port: u16 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(8765);
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let authority = listener.local_addr()?.to_string();
    let mut random = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let token = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let view = View {
        device: device.clone(),
        local: None,
        channel: None,
        contacts: Vec::new(),
        incomplete: Vec::new(),
        stage: "idle".into(),
        message: "Connect your radio, then discover nearby vertices.".into(),
        target: None,
        release: None,
        accepted: 0,
        committed: 0,
        total: 0,
        prepared: 0,
        changes: 0,
        lost_changes: 0,
        busy: false,
    };
    let (progress_sender, _) = watch::channel(view.clone());
    println!("Radio: {device}");
    let state = Arc::new(Maintenance {
        device,
        authority: authority.clone(),
        token,
        view: Mutex::new(view),
        events: progress_sender,
        busy: AtomicBool::new(false),
    });
    let app = Router::new()
        .route("/", get(index))
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    include_str!("../ui/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css")],
                    include_str!("../ui/style.css"),
                )
            }),
        )
        .route("/events", get(events))
        .route("/discover", post(discover))
        .route("/upload/{owner}", post(upload))
        .layer(DefaultBodyLimit::max(MAX_PACKAGE_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), local_request))
        .with_state(state);
    println!("Lattice maintenance: http://{authority}");
    // Normal process termination closes USB. The receiver's durable checkpoints
    // support resumption; an open SSE window must not trap Ctrl-C indefinitely.
    axum::serve(listener, app).await?;
    Ok(())
}

async fn local_request(
    State(state): State<Arc<Maintenance>>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let origin = format!("http://{}", state.authority);
    let authorized = headers.get(header::HOST).and_then(|h| h.to_str().ok())
        == Some(state.authority.as_str())
        && headers
            .get(header::ORIGIN)
            .is_none_or(|h| h.to_str().ok() == Some(origin.as_str()))
        && (request.method() == "GET"
            || headers
                .get("x-lattice-session")
                .and_then(|h| h.to_str().ok())
                == Some(state.token.as_str()));
    if !authorized {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert(
        "content-security-policy",
        "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
            .parse()
            .unwrap(),
    );
    response
}
async fn index(State(state): State<Arc<Maintenance>>) -> Html<String> {
    Html(include_str!("../ui/index.html").replace("SESSION_TOKEN", &state.token))
}
async fn events(State(state): State<Arc<Maintenance>>) -> impl IntoResponse {
    Sse::new(
        WatchStream::new(state.events.subscribe())
            .map(|view| Ok::<_, Infallible>(SseEvent::default().json_data(view).unwrap())),
    )
    .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
}
async fn discover(State(state): State<Arc<Maintenance>>) -> Result<StatusCode, ApiError> {
    let job = state.begin("discovering")?;
    state.publish(|v| {
        v.target = None;
        v.release = None;
        v.total = 0;
        v.accepted = 0;
        v.committed = 0;
        v.prepared = 0;
    });
    tokio::task::spawn_blocking(move || {
        let _job = job;
        match Client::open(&state.device).and_then(|mut client| client.discover(32)) {
            Ok(found) => state.publish(|v| {
                v.local = Some(format!("{:016x}", found.local));
                v.channel = Some(found.channel);
                v.contacts = found
                    .contacts
                    .into_iter()
                    .map(|c| Contact {
                        address: format!("{:016x}", c.address),
                        via: format!("{:016x}", c.via),
                        responded: c.responded,
                    })
                    .collect();
                v.incomplete = found.incomplete;
                v.stage = "discovered".into();
                let count = v.contacts.len();
                let noun = if count == 1 { "vertex" } else { "vertices" };
                v.message = format!("{count} {noun} discovered. Select one to update.");
            }),
            Err(error) => {
                state.publish(|v| {
                    v.local = None;
                    v.contacts.clear();
                });
                state.fail(error);
            }
        }
    });
    Ok(StatusCode::ACCEPTED)
}
async fn upload(
    State(state): State<Arc<Maintenance>>,
    Path(owner): Path<String>,
    bytes: Bytes,
) -> Result<StatusCode, ApiError> {
    let address = u64::from_str_radix(&owner, 16)
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "invalid vertex address".into()))?;
    let owner = format!("{address:016x}");
    let local = {
        let view = state.view.lock().unwrap();
        if !view
            .contacts
            .iter()
            .any(|c| c.address == owner && c.responded)
        {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "discover and select a responding remote vertex first".into(),
            ));
        }
        view.local
            .clone()
            .ok_or_else(|| ApiError(StatusCode::CONFLICT, "discover the network first".into()))?
    };
    let package = Package::parse(bytes.to_vec())
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
    if !package.measured {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "this package has no measured activation costs; it requires explicit local maintenance"
                .into(),
        ));
    }
    let job = state.begin("offering")?;
    state.publish(|v| {
        v.target = Some(owner);
        v.release = Some(package.release.to_string());
        v.total = package.image_bytes();
        v.accepted = 0;
        v.committed = 0;
        v.prepared = 0;
    });
    tokio::task::spawn_blocking(move || {
        let _job = job;
        let transfer = || -> io::Result<()> {
            let mut client = Client::open(&state.device)?;
            match client.status()? {
                Packet::Ready { address, .. } if format!("{address:016x}") == local => {}
                _ => return Err(io::Error::other("attached radio changed; discover again")),
            }
            let mut last = Instant::now() - Duration::from_secs(1);
            let mut last_phase = None;
            client.upload(address, &package, |event| match event {
                UploadEvent::Observed(event) => {
                    let mut view = state.view.lock().unwrap();
                    match event {
                        Event::Changed(_) => view.changes += 1,
                        Event::Lagged { dropped, .. } => view.lost_changes += u64::from(dropped),
                    }
                }
                UploadEvent::Progress(progress) => {
                    let p = progress.receiver;
                    if last_phase != Some(p.phase) || last.elapsed() >= Duration::from_millis(200) {
                        last_phase = Some(p.phase);
                        last = Instant::now();
                        state.publish(|v| {
                            v.stage = format!("{:?}", p.phase).to_lowercase();
                            v.message = if p.status == lattice_peer::wire::update::Status::Busy {
                                "Vertex is servicing other work; transfer will continue.".into()
                            } else {
                                String::new()
                            };
                            v.accepted = p.accepted;
                            v.committed = p.committed;
                            v.prepared = p.prepared;
                        });
                    }
                }
            })?;
            state.publish(|v| { v.stage = "ready".into(); v.accepted = package.image_bytes(); v.committed = package.image_bytes(); v.message = "Image verified by the vertex. It will choose its activation time. Boot confirmation is not reported through this interface yet.".into(); });
            Ok(())
        };
        if let Err(error) = transfer() {
            state.fail(error);
        }
    });
    Ok(StatusCode::ACCEPTED)
}
