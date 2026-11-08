//! The admin HTTP/WebSocket API and Prometheus metrics (axum).
//!
//! Enabled by `[Admin] Listen = ip:port` in `global.conf`. With
//! `[Admin] Token` set, every request must carry `Authorization: Bearer
//! <token>`; without a token the API only listens on a loopback address.
//!
//! - `GET /metrics`: Prometheus text (counters from [`Metrics`], plus player,
//!   arena and module gauges).
//! - `GET /api/status`, `/api/players`, `/api/arenas`, `/api/modules`
//! - `POST /api/message` `{"text": "...", "arena": "name"}` (arena optional:
//!   all arenas)
//! - `POST /api/players/{pid}/kick`
//! - `POST /api/reload`: re-read changed config files
//! - `POST /api/modules/{name}` / `DELETE /api/modules/{name}`: load / unload
//! - `GET /api/events`: WebSocket stream of JSON events (logins, arena
//!   changes, kills, public chat) from the `admin` module.
//!
//! Handlers run their zone work on the game thread through
//! [`ZoneHandle::run`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::metrics::Metrics;
use crate::module::{Module, ModuleError};
use crate::player::{PlayerId, PlayerStatus};
use crate::registry::{Owner, Scope};
use crate::runtime::ZoneHandle;
use crate::zone::Zone;

/// Shared state of the admin server.
#[derive(Clone)]
pub struct AdminState {
    handle: ZoneHandle,
    metrics: Arc<Metrics>,
    events: Option<broadcast::Sender<Arc<str>>>,
    token: Option<Arc<str>>,
    started: Instant,
}

/// Admin settings read from `[Admin]`.
#[derive(Debug, Clone)]
pub struct AdminConfig {
    /// Where to listen.
    pub listen: SocketAddr,
    /// Bearer token, if any.
    pub token: Option<String>,
}

impl AdminConfig {
    /// Reads `[Admin]`; `None` when the API is off or misconfigured.
    pub fn from_zone(zone: &Zone) -> Option<Self> {
        let g = zone.cfg.global();
        let listen = g.get("Admin", "Listen")?.trim();
        let listen: SocketAddr = match listen.parse() {
            Ok(a) => a,
            Err(_) => {
                tracing::error!(target: "admin", "bad Admin:Listen '{listen}'; admin API disabled");
                return None;
            }
        };
        let token = g
            .get("Admin", "Token")
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        if token.is_none() && !listen.ip().is_loopback() {
            tracing::error!(target: "admin", "Admin:Listen is not a loopback address and no Admin:Token is set; admin API disabled");
            return None;
        }
        Some(Self { listen, token })
    }
}

/// Builds the router.
pub fn router(
    handle: ZoneHandle,
    metrics: Arc<Metrics>,
    events: Option<broadcast::Sender<Arc<str>>>,
    token: Option<String>,
) -> Router {
    let state = AdminState {
        handle,
        metrics,
        events,
        token: token.map(Arc::from),
        started: Instant::now(),
    };
    Router::new()
        .route("/metrics", get(metrics_page))
        .route("/api/status", get(status))
        .route("/api/players", get(players))
        .route("/api/arenas", get(arenas))
        .route("/api/modules", get(modules))
        .route(
            "/api/modules/{name}",
            post(load_module).delete(unload_module),
        )
        .route("/api/message", post(send_message))
        .route("/api/players/{pid}/kick", post(kick))
        .route("/api/reload", post(reload))
        .route("/api/events", get(events_ws))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state)
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth(State(st): State<AdminState>, req: Request, next: Next) -> Response {
    if let Some(token) = &st.token {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| same(t.as_bytes(), token.as_bytes()));
        if !ok {
            return (StatusCode::UNAUTHORIZED, "missing or wrong bearer token").into_response();
        }
    }
    next.run(req).await
}

/// Runs `f` on the game thread and waits for its result.
async fn on_zone<T: Send + 'static>(
    st: &AdminState,
    f: impl FnOnce(&mut Zone) -> T + Send + 'static,
) -> Result<T, StatusCode> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if !st.handle.run(move |z| {
        let _ = tx.send(f(z));
    }) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    match tokio::time::timeout(Duration::from_secs(5), rx).await {
        Ok(Ok(v)) => Ok(v),
        _ => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

#[derive(Serialize)]
struct Status {
    uptime_seconds: u64,
    players: usize,
    playing: usize,
    arenas: usize,
    modules: usize,
}

async fn status(State(st): State<AdminState>) -> Result<Json<Status>, StatusCode> {
    let uptime_seconds = st.started.elapsed().as_secs();
    on_zone(&st, move |z| Status {
        uptime_seconds,
        players: z.players.iter().count(),
        playing: z
            .players
            .iter()
            .filter(|p| p.status == PlayerStatus::Playing)
            .count(),
        arenas: z.arenas.iter().count(),
        modules: z.modules.loaded().count(),
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
struct PlayerInfo {
    pid: u16,
    name: String,
    squad: String,
    arena: Option<String>,
    ship: u8,
    freq: u16,
    kind: String,
    status: String,
    authenticated: bool,
}

async fn players(State(st): State<AdminState>) -> Result<Json<Vec<PlayerInfo>>, StatusCode> {
    on_zone(&st, |z| {
        z.players
            .iter()
            .map(|p| PlayerInfo {
                pid: p.id.0,
                name: p.name.clone(),
                squad: p.squad.clone(),
                arena: p
                    .arena
                    .and_then(|a| z.arenas.get(a))
                    .map(|a| a.name.clone()),
                ship: p.ship.0,
                freq: p.freq,
                kind: format!("{:?}", p.kind),
                status: format!("{:?}", p.status),
                authenticated: p.authenticated,
            })
            .collect()
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
struct ArenaInfo {
    name: String,
    players: usize,
    in_ships: usize,
}

fn arena_list(z: &Zone) -> Vec<ArenaInfo> {
    z.arenas
        .iter()
        .map(|a| ArenaInfo {
            name: a.name.clone(),
            players: z.players.in_arena(a.id).count(),
            in_ships: z
                .players
                .in_arena(a.id)
                .filter(|p| !p.ship.is_spec())
                .count(),
        })
        .collect()
}

async fn arenas(State(st): State<AdminState>) -> Result<Json<Vec<ArenaInfo>>, StatusCode> {
    on_zone(&st, |z| arena_list(z)).await.map(Json)
}

async fn modules(State(st): State<AdminState>) -> Result<Json<Vec<String>>, StatusCode> {
    on_zone(&st, |z| z.modules.loaded().map(str::to_owned).collect())
        .await
        .map(Json)
}

async fn load_module(
    State(st): State<AdminState>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let r = on_zone(&st, move |z| {
        crate::module::insert(z, &name)
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|s| (s, String::new()))?;
    r.map(|()| StatusCode::NO_CONTENT)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}

async fn unload_module(
    State(st): State<AdminState>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let r = on_zone(&st, move |z| {
        crate::module::unload(z, &name).map_err(|e| e.to_string())
    })
    .await
    .map_err(|s| (s, String::new()))?;
    r.map(|()| StatusCode::NO_CONTENT)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}

#[derive(Deserialize)]
struct MessageReq {
    text: String,
    arena: Option<String>,
}

async fn send_message(
    State(st): State<AdminState>,
    Json(req): Json<MessageReq>,
) -> Result<StatusCode, StatusCode> {
    if req.text.is_empty() || req.text.len() > 250 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let sent = on_zone(&st, move |z| {
        let Some(chat) = z.iface::<dyn crate::modules::chat::Chat>() else {
            return false;
        };
        let targets: Vec<_> = z
            .arenas
            .iter()
            .filter(|a| req.arena.as_ref().is_none_or(|n| a.name == *n))
            .map(|a| a.id)
            .collect();
        for a in &targets {
            chat.send_arena_message(z, *a, &req.text);
        }
        !targets.is_empty()
    })
    .await?;
    Ok(if sent {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

async fn kick(
    State(st): State<AdminState>,
    Path(pid): Path<u16>,
) -> Result<StatusCode, StatusCode> {
    let found = on_zone(&st, move |z| {
        let p = PlayerId(pid);
        if z.players.get(p).is_none() {
            return false;
        }
        tracing::info!(target: "admin", pid, "kicked through the admin API");
        crate::modules::core::kick(z, p);
        true
    })
    .await?;
    Ok(if found {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

#[derive(Serialize)]
struct Reloaded {
    files: usize,
}

async fn reload(State(st): State<AdminState>) -> Result<Json<Reloaded>, StatusCode> {
    on_zone(&st, |z| Reloaded {
        files: crate::modules::arenaman::reload_config(z),
    })
    .await
    .map(Json)
}

async fn metrics_page(State(st): State<AdminState>) -> Result<Response, StatusCode> {
    let mut out = String::with_capacity(4096);
    st.metrics.render(&mut out);
    let uptime = st.started.elapsed().as_secs_f64();
    let gauges = on_zone(&st, move |z| {
        use std::fmt::Write;
        let mut s = String::new();
        let mut by_status: std::collections::BTreeMap<String, usize> = Default::default();
        let mut by_kind: std::collections::BTreeMap<String, usize> = Default::default();
        for p in z.players.iter() {
            *by_status.entry(format!("{:?}", p.status)).or_default() += 1;
            *by_kind.entry(format!("{:?}", p.kind)).or_default() += 1;
        }
        let _ = writeln!(s, "# HELP oss_players Players by connection status.\n# TYPE oss_players gauge");
        for (k, v) in &by_status {
            let _ = writeln!(s, "oss_players{{status=\"{k}\"}} {v}");
        }
        let _ = writeln!(s, "# HELP oss_players_by_client Players by client type.\n# TYPE oss_players_by_client gauge");
        for (k, v) in &by_kind {
            let _ = writeln!(s, "oss_players_by_client{{client=\"{k}\"}} {v}");
        }
        let _ = writeln!(s, "# HELP oss_arena_players Players in each arena.\n# TYPE oss_arena_players gauge");
        for a in arena_list(z) {
            let name: String = a.name.chars().filter(|c| *c != '"' && *c != '\\').collect();
            let _ = writeln!(s, "oss_arena_players{{arena=\"{name}\"}} {}", a.players);
        }
        let _ = writeln!(
            s,
            "# HELP oss_arenas Arenas.\n# TYPE oss_arenas gauge\noss_arenas {}\n# HELP oss_modules Loaded modules.\n# TYPE oss_modules gauge\noss_modules {}\n# HELP oss_timers Pending timers.\n# TYPE oss_timers gauge\noss_timers {}",
            z.arenas.iter().count(),
            z.modules.loaded().count(),
            z.timers.len()
        );
        s
    })
    .await?;
    out.push_str(&gauges);
    out.push_str(&format!(
        "# HELP oss_uptime_seconds Seconds since the admin API started.\n# TYPE oss_uptime_seconds gauge\noss_uptime_seconds {uptime}\n"
    ));
    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response())
}

async fn events_ws(State(st): State<AdminState>, ws: WebSocketUpgrade) -> Response {
    let Some(tx) = st.events.clone() else {
        return (StatusCode::NOT_FOUND, "the admin module isn't loaded").into_response();
    };
    ws.on_upgrade(move |socket| stream_events(socket, tx.subscribe()))
}

async fn stream_events(mut socket: WebSocket, mut rx: broadcast::Receiver<Arc<str>>) {
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(text) => {
                    if socket.send(Message::Text(text.as_ref().into())).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let note = format!("{{\"type\":\"lagged\",\"missed\":{n}}}");
                    if socket.send(Message::Text(note.into())).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            },
        }
    }
}

// ---- the event source --------------------------------------------------------

/// The admin event stream's sender (set up by the `admin` module).
pub struct AdminEvents(pub broadcast::Sender<Arc<str>>);

fn emit(zone: &Zone, v: serde_json::Value) {
    if let Some(e) = zone.res.get::<AdminEvents>()
        && e.0.receiver_count() > 0
    {
        let _ = e.0.send(Arc::from(v.to_string()));
    }
}

fn player_name(zone: &Zone, p: PlayerId) -> String {
    zone.players
        .get(p)
        .map(|x| x.name.clone())
        .unwrap_or_default()
}

fn arena_name(zone: &Zone, a: Option<crate::arena::ArenaId>) -> Option<String> {
    a.and_then(|a| zone.arenas.get(a)).map(|a| a.name.clone())
}

/// The `admin` module: feeds zone events to the admin API's WebSocket.
pub struct AdminModule;

impl Module for AdminModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        use crate::modules::core::{Action, PlayerAction};
        use serde_json::json;
        let (tx, _) = broadcast::channel(1024);
        zone.res.insert(AdminEvents(tx));
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            let kind = match ev.action {
                Action::Connect => "login",
                Action::Disconnect => "logout",
                Action::EnterArena => "enter_arena",
                Action::LeaveArena => "leave_arena",
                _ => return,
            };
            emit(
                z,
                json!({"type": kind, "pid": ev.player.0, "name": player_name(z, ev.player),
                       "arena": arena_name(z, ev.arena)}),
            );
        });
        zone.on::<crate::modules::game::KillAnnounced>(me, Scope::Global, |z, ev| {
            emit(
                z,
                json!({"type": "kill", "arena": arena_name(z, Some(ev.arena)),
                       "killer": player_name(z, ev.killer), "killed": player_name(z, ev.killed),
                       "points": ev.points, "flags": ev.flags}),
            );
        });
        zone.on::<crate::modules::chat::ChatMessage>(me, Scope::Global, |z, ev| {
            use oss_proto::common::chat_type;
            // public chat only: private, team and channel messages stay private
            if !matches!(ev.kind, chat_type::PUBLIC | chat_type::PUBLIC_MACRO) {
                return;
            }
            emit(
                z,
                json!({"type": "chat", "arena": arena_name(z, ev.arena),
                       "name": player_name(z, ev.from), "text": ev.text}),
            );
        });
        Ok(())
    }

    fn unload(&self, zone: &mut Zone, _me: Owner) {
        zone.res.remove::<AdminEvents>();
    }
}
