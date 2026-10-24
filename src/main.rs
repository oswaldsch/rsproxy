use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use config::Config;
use env_logger::Env;
use helpers::{LogEntry, clean_request, make_plain_response};
use http_body_util::{BodyExt, Full};
use hyper::StatusCode;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use log::{info, warn};
use tokio::net::{TcpListener, TcpStream};

mod config;
mod helpers;

struct Proxy {
    config: Config,
}

impl Proxy {
    pub async fn route(
        &self,
        request: Request<hyper::body::Incoming>,
        client: SocketAddr,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        let incoming_ts = Instant::now();
        let mut log_entry = LogEntry {
            client,
            host: None,
            method: request.method().clone(),
            path: request.uri().path().to_string(),
            version: request.version(),
            status: None,
            upstream: None,
            duration: None,
        };
        let Some(host) = request.headers().get("host") else {
            log_entry.duration = Some(incoming_ts.elapsed());
            log_entry.status = Some(StatusCode::BAD_REQUEST);
            info!("no Host header sent");
            info!("{}", log_entry);
            return Ok(make_plain_response(
                "400 Bad Request: No Host header sent!",
                StatusCode::BAD_REQUEST,
            ));
        };
        let Ok(host) = host.to_str() else {
            log_entry.duration = Some(incoming_ts.elapsed());
            log_entry.status = Some(StatusCode::BAD_REQUEST);
            info!("Host header is not an ascii string");
            info!("{}", log_entry);
            return Ok(make_plain_response(
                "400 Bad Request: Invalid Host header",
                StatusCode::BAD_REQUEST,
            ));
        };
        log_entry.host = Some(host.to_string());
        if self.config.hosts.contains_key(host) {
            let host = host.to_string();
            log_entry.upstream = Some(self.config.hosts[&host].forward.clone());
            let response = self.forward_request(request, host).await;
            if let Ok(response) = response {
                let (response_head, response_body) = response.into_parts();
                if let Ok(response_body) = response_body.collect().await {
                    // FIXME: proxy loads entire response into ram instead of streaming
                    let response_body = Full::new(response_body.to_bytes());
                    let response = Response::from_parts(response_head, response_body);
                    log_entry.duration = Some(incoming_ts.elapsed());
                    log_entry.status = Some(response.status());
                    info!("{}", log_entry);
                    Ok(response)
                } else {
                    log_entry.duration = Some(incoming_ts.elapsed());
                    log_entry.status = Some(StatusCode::BAD_GATEWAY);
                    warn!("upstream unreachable");
                    info!("{}", log_entry);
                    Ok(make_plain_response(
                        "502 Bad Gateway",
                        StatusCode::BAD_GATEWAY,
                    ))
                }
            } else {
                log_entry.duration = Some(incoming_ts.elapsed());
                log_entry.status = Some(StatusCode::BAD_GATEWAY);
                warn!("upstream unreachable");
                info!("{}", log_entry);
                Ok(make_plain_response(
                    "502 Bad Gateway",
                    StatusCode::BAD_GATEWAY,
                ))
            }
        } else {
            log_entry.duration = Some(incoming_ts.elapsed());
            log_entry.status = Some(StatusCode::MISDIRECTED_REQUEST);
            info!("no route for host '{}'", host);
            info!("{}", log_entry);
            Ok(make_plain_response(
                format!("421 Misdirected Request: No route for host '{}'", host),
                StatusCode::MISDIRECTED_REQUEST,
            ))
        }
    }

    async fn forward_request(
        &self,
        request: Request<hyper::body::Incoming>,
        host: String,
    ) -> Result<Response<Incoming>, Box<dyn std::error::Error + Send + Sync>> {
        let stream = TcpStream::connect((
            self.config.hosts[&host].forward.clone(),
            self.config.hosts[&host].port,
        ))
        .await?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                println!("Connection failed: {:?}", err);
            }
        });

        let cleaned_request = if self.config.hosts[&host].external {
            clean_request(request, Some(self.config.hosts[&host].forward.clone()))?
        } else {
            clean_request(request, None)
                .expect("clean_request without host override should never fail")
        };

        let response = sender.send_request(cleaned_request).await?;
        Ok(response)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let config = Config::load("./config.toml").unwrap_or_else(|err| {
        eprintln!("failed to load config: {}", err);
        std::process::exit(1)
    });

    let proxy = Arc::new(Proxy { config });

    let addr = SocketAddr::from(([127, 0, 0, 1], proxy.config.listen));
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, client) = listener.accept().await?;

        let io = TokioIo::new(stream);
        let proxy = Arc::clone(&proxy);

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(|request| proxy.route(request, client)))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}
