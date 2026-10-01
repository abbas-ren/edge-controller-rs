//! Backend heartbeat connection.
//!
//! The backend receives JSON encoded in binary WebSocket frames.
//! Incoming application messages are observed but are not executed as
//! hardware commands; the supplied C client only prints those messages.

use crate::{
    error::{AppError, AppResult},
    observability::metrics::{global_metrics, MetricSampler},
    state::AppState,
};

use futures::{SinkExt, StreamExt};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    net::TcpStream,
    time::{self, Instant, MissedTickBehavior},
};

use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        client::IntoClientRequest,
        http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderValue},
        protocol::WebSocketConfig,
        Message,
    },
    MaybeTlsStream, WebSocketStream,
};

use tracing::{debug, info, warn};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct ConnectionMetricGuard;

impl Drop for ConnectionMetricGuard {
    fn drop(&mut self) {
        global_metrics().set_websocket_connected(false);
    }
}

const SUBPROTOCOL: &str = "web-cli-protocol";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const PONG_TIMEOUT: Duration = Duration::from_secs(70);

fn next_retry_delay(current: Duration, session_elapsed: Duration) -> Duration {
    if session_elapsed >= Duration::from_secs(30) {
        Duration::from_secs(2)
    } else {
        (current * 2).min(Duration::from_secs(60))
    }
}

async fn send(socket: &mut Socket, message: Message) -> AppResult<()> {
    time::timeout(WRITE_TIMEOUT, socket.send(message))
        .await
        .map_err(|_| AppError::Msg("WebSocket write timed out".into()))?
        .map_err(|error| AppError::Msg(format!("WebSocket write failed: {error}")))
}

fn connection_request(
    state: &AppState,
    uid: &str,
) -> AppResult<tokio_tungstenite::tungstenite::http::Request<()>> {
    let mut url = reqwest::Url::parse(&format!(
        "ws://{}:{}/ws",
        state.cfg.server_ip, state.cfg.ws_port
    ))
    .map_err(|error| AppError::Msg(format!("invalid WebSocket URL: {error}")))?;

    url.query_pairs_mut().append_pair("deviceControllerId", uid);

    let mut request = url.as_str().into_client_request().map_err(|error| {
        AppError::Msg(format!("WebSocket request construction failed: {error}"))
    })?;

    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(SUBPROTOCOL),
    );

    Ok(request)
}

async fn connection_session(
    state: &AppState,
    uid: &str,
    sampler: Arc<Mutex<MetricSampler>>,
) -> AppResult<()> {
    let request = connection_request(state, uid)?;

    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(64 * 1024);
    config.max_frame_size = Some(64 * 1024);

    let (mut socket, response) = time::timeout(
        CONNECT_TIMEOUT,
        connect_async_with_config(request, Some(config), true),
    )
    .await
    .map_err(|_| AppError::Msg("WebSocket connection timed out".into()))?
    .map_err(|error| AppError::Msg(format!("WebSocket connection failed: {error}")))?;

    // Some existing servers omit subprotocol selection. Permit omission,
    // but reject an explicitly different protocol.
    if let Some(selected) = response.headers().get(SEC_WEBSOCKET_PROTOCOL) {
        if selected.as_bytes() != SUBPROTOCOL.as_bytes() {
            return Err(AppError::Msg(
                "backend selected an unexpected WebSocket subprotocol".into(),
            ));
        }
    } else {
        warn!("backend did not explicitly select the requested subprotocol");
    }

    info!("WebSocket connected");
    global_metrics().set_websocket_connected(true);
    global_metrics().record_websocket_event("connected");
    let _connection_metric = ConnectionMetricGuard;

    let start = Instant::now() + HEARTBEAT_INTERVAL;
    let mut heartbeat = time::interval_at(start, HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Inspect service shutdown and identity changes without waiting for
    // the full heartbeat interval.
    let mut lifecycle = time::interval(Duration::from_secs(1));
    lifecycle.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut last_pong = Instant::now();

    loop {
        tokio::select! {
            _ = lifecycle.tick() => {
                let shutting_down = state.jobs.is_closing();

                let current_uid = state.controller.read().await.uid.clone();

                if shutting_down || current_uid.as_deref() != Some(uid) {
                    // Best-effort close. Dropping the stream also closes
                    // the connection if the peer does not respond.
                    let _ = send(&mut socket, Message::Close(None)).await;
                    return Ok(());
                }

                // A missed heartbeat usually means the peer is unreachable or the
                // socket is half-dead; treat that as a terminal session problem.
                if last_pong.elapsed() > PONG_TIMEOUT {
                    return Err(AppError::Msg(
                        "WebSocket pong deadline exceeded".into(),
                    ));
                }
            }

            _ = heartbeat.tick() => {
                // Send a keepalive on a fixed cadence so the upstream backend can
                // confirm that the controller is still alive and responsive.
                let ping_payload = Vec::new();
                debug!("sending WebSocket ping heartbeat");
                send(&mut socket, Message::Ping(ping_payload.into())).await?;

                let ip = state.controller.read().await.board_ip.clone();
                let uid = uid.to_owned();
                let sampler = sampler.clone();

                let sampled = tokio::task::spawn_blocking(move || {
                    let mut sampler = sampler.lock().map_err(|_| {
                        AppError::Msg("metrics sampler mutex was poisoned".into())
                    })?;

                    sampler.heartbeat(uid, ip)
                })
                .await
                .map_err(|error| {
                    AppError::Msg(format!("metrics worker failed: {error}"))
                })?;

                match sampled {
                    Ok(payload) => {
                        let bytes = serde_json::to_vec(&payload)?;
                        debug!(bytes = bytes.len(), "sending heartbeat metrics payload");
                        send(&mut socket, Message::Binary(bytes.into())).await?;
                        global_metrics().record_websocket_event("heartbeat_sent");
                    }
                    Err(error) => {
                        // Keep the transport alive, but do not send a
                        // fabricated successful metrics sample.
                        warn!(%error, "heartbeat metrics unavailable");
                    }
                }
            }

            incoming = socket.next() => {
                match incoming {
                    Some(Ok(Message::Pong(data))) => {
                        last_pong = Instant::now();
                        global_metrics().record_websocket_event("pong_received");
                        debug!(bytes = data.len(), "received WebSocket pong");
                    }

                    Some(Ok(Message::Ping(data))) => {
                        // Respond immediately to a ping. The peer must see a pong to
                        // consider the socket healthy; simply flushing is not enough.
                        debug!(bytes = data.len(), "received WebSocket ping; replying with pong");
                        send(&mut socket, Message::Pong(data)).await?;
                        global_metrics().record_websocket_event("ping_received");
                        last_pong = Instant::now();
                    }

                    Some(Ok(Message::Text(message))) => {
                        // Avoid logging remote content or credentials.
                        debug!(
                            bytes = message.len(),
                            "received backend text message"
                        );
                        global_metrics().record_websocket_event("text_received");
                    }

                    Some(Ok(Message::Binary(message))) => {
                        debug!(
                            bytes = message.len(),
                            "received backend binary message"
                        );
                        global_metrics().record_websocket_event("binary_received");
                    }

                    Some(Ok(Message::Close(_))) => {
                        info!("backend requested WebSocket close");
                        return Ok(());
                    }

                    Some(Ok(_)) => {}

                    Some(Err(error)) => {
                        return Err(AppError::Msg(format!(
                            "WebSocket receive failed: {error}"
                        )));
                    }

                    None => {
                        info!("WebSocket stream ended; reconnecting");
                        return Ok(());
                    }
                }
            }
        }
    }
}

pub async fn websocket_loop(state: Arc<AppState>) {
    let sampler = Arc::new(Mutex::new(MetricSampler::default()));
    let mut retry_delay = Duration::from_secs(2);

    loop {
        if state.jobs.is_closing() {
            break;
        }

        let uid = state.controller.read().await.uid.clone();

        let Some(uid) = uid.filter(|uid| !uid.is_empty()) else {
            time::sleep(Duration::from_secs(1)).await;
            continue;
        };

        let started = Instant::now();

        let reconnect_event = match connection_session(&state, &uid, sampler.clone()).await {
            Ok(()) => "reconnect_clean",
            Err(error) => {
                warn!(%error, "WebSocket session ended");
                "reconnect_error"
            }
        };

        if state.jobs.is_closing() {
            break;
        }

        global_metrics().record_websocket_event(reconnect_event);

        // Reset backoff after a reasonably stable connection. Repeated
        // immediate failures otherwise back off to at most one minute.
        let next_delay = next_retry_delay(retry_delay, started.elapsed());
        info!(
            uid = %uid,
            retry_after = ?next_delay,
            session_elapsed = ?started.elapsed(),
            "WebSocket session ended; retrying connection"
        );

        time::sleep(next_delay).await;
        retry_delay = next_delay;
    }

    info!("WebSocket service stopped");
}

#[cfg(test)]
mod tests {
    use super::next_retry_delay;
    use std::time::Duration;

    #[test]
    fn reconnect_backoff_resets_after_a_stable_session() {
        assert_eq!(
            next_retry_delay(Duration::from_secs(2), Duration::from_secs(31)),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_retry_delay(Duration::from_secs(2), Duration::from_secs(10)),
            Duration::from_secs(4)
        );
        assert_eq!(
            next_retry_delay(Duration::from_secs(60), Duration::from_secs(10)),
            Duration::from_secs(60)
        );
    }
}
