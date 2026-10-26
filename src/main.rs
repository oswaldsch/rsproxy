use std::convert::Infallible;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use config::Config;
use env_logger::Env;
use helpers::{LogEntry, clean_request, make_plain_response};
use http::{HeaderValue, Version, header};
use http_body_util::{BodyExt, Full};
use hyper::StatusCode;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use log::{info, warn};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio::net::{TcpListener, TcpStream};
use tokio::try_join;

use crate::helpers::{canonical_host, check_via_header, get_version_literal};

mod config;
mod helpers;

#[derive(Debug)]
struct UnsupportedVersion(Version);

impl Display for UnsupportedVersion {
    fn fmt(&self, formatter: &mut Formatter) -> Result<(), std::fmt::Error> {
        write!(formatter, "unsupported version {:?}", self.0)
    }
}

impl Error for UnsupportedVersion {}

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

        if let Some(response) = check_via_header(&request, client) {
            return Ok(response);
        }

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
        let host = canonical_host(host, false);
        if let Err(err) = host {
            log_entry.duration = Some(incoming_ts.elapsed());
            log_entry.status = Some(StatusCode::BAD_REQUEST);
            info!("Host header invalid: {}", err);
            info!("{}", log_entry);
            return Ok(make_plain_response(
                "400 Bad Request: Invalid Host header",
                StatusCode::BAD_REQUEST,
            ));
        }
        let host = host.unwrap();
        log_entry.host = Some(host.to_string());
        if self.config.hosts.contains_key(&host) {
            log_entry.upstream = Some(self.config.hosts[&host].forward.clone());
            let response = self.forward_request(request, &host).await;
            if let Ok(response) = response {
                let (response_head, response_body) = response.into_parts();
                let response_body = response_body.collect().await;
                if let Ok(response_body) = response_body {
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
                    warn!(
                        "upstream at '{}' unreachable: {}",
                        self.config.hosts[&host].forward,
                        response_body.err().unwrap()
                    );
                    info!("{}", log_entry);
                    Ok(make_plain_response(
                        "502 Bad Gateway",
                        StatusCode::BAD_GATEWAY,
                    ))
                }
            } else {
                log_entry.duration = Some(incoming_ts.elapsed());
                log_entry.status = Some(StatusCode::BAD_GATEWAY);
                warn!(
                    "upstream at {} unreachable: {}",
                    self.config.hosts[&host].forward,
                    response.err().unwrap()
                );
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
        host: &String,
    ) -> Result<Response<Incoming>, Box<dyn std::error::Error + Send + Sync>> {
        let stream = TcpStream::connect((
            self.config.hosts[host].forward.clone(),
            self.config.hosts[host].port,
        ))
        .await?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                println!("Connection failed: {:?}", err);
            }
        });

        let version = request.version();
        let Some(version) = get_version_literal(version) else {
            return Err(UnsupportedVersion(version).into());
        };

        let mut cleaned_request = if self.config.hosts[host].external {
            clean_request(request, Some(self.config.hosts[host].forward.clone()))?
        } else {
            clean_request(request, None)
                .expect("clean_request without host override should never fail")
        };

        let headers = cleaned_request.headers_mut();
        headers.append(
            header::VIA,
            HeaderValue::from_str(&format!("{} rsproxy", version))?,
        );

        let mut response = sender.send_request(cleaned_request).await?;
        let headers = response.headers_mut();
        headers.append(
            header::VIA,
            HeaderValue::from_str(&format!("{} rsproxy", version))?,
        );
        Ok(response)
    }

    async fn upgrade_https(
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

        if let Some(response) = check_via_header(&request, client) {
            return Ok(response);
        }

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
        let host = canonical_host(host, false);
        if let Err(err) = host {
            log_entry.duration = Some(incoming_ts.elapsed());
            log_entry.status = Some(StatusCode::BAD_REQUEST);
            info!("Host header invalid: {}", err);
            info!("{}", log_entry);
            return Ok(make_plain_response(
                "400 Bad Request: Invalid Host header",
                StatusCode::BAD_REQUEST,
            ));
        }
        let host = host.unwrap() + ":" + &self.config.listen_https.to_string(); // TODO: clean this up
        log_entry.host = Some(host.to_string());

        let new_uri_builder = http::uri::Builder::new()
            .scheme(http::uri::Scheme::HTTPS)
            .authority(host)
            .path_and_query(
                request
                    .uri()
                    .path_and_query()
                    .unwrap_or(&http::uri::PathAndQuery::from_static("/"))
                    .as_str(),
            );
        let new_uri = new_uri_builder
            .build()
            .expect("host should already be normalized");

        let mut response =
            make_plain_response("Redirecting to https", StatusCode::MOVED_PERMANENTLY);
        let headers = response.headers_mut();
        headers.insert(
            header::LOCATION,
            HeaderValue::from_str(&new_uri.to_string()).unwrap(),
        );

        Ok(response)
    }
}

async fn http_listener_loop(
    proxy: Arc<Proxy>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = SocketAddr::from(([127, 0, 0, 1], proxy.config.listen_http));
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, client) = listener.accept().await?;

        let proxy = Arc::clone(&proxy);

        tokio::task::spawn(async move {
            let io = TokioIo::new(stream);
            if let Err(err) = http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(|request| proxy.upgrade_https(request, client)),
                )
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}

async fn https_listener_loop(
    proxy: Arc<Proxy>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let certs: Vec<CertificateDer> =
        CertificateDer::pem_file_iter("oswald.localhost.pem")?.collect::<Result<_, _>>()?;
    let key = PrivateKeyDer::from_pem_file("oswald.localhost-key.pem")?;

    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));

    let addr = SocketAddr::from(([127, 0, 0, 1], proxy.config.listen_https));
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, client) = listener.accept().await?;

        let proxy = Arc::clone(&proxy);
        let acceptor = acceptor.clone();

        tokio::task::spawn(async move {
            let tls = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("handshake failed: {e}");
                    return;
                }
            };
            let io = TokioIo::new(tls);
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(|request| proxy.route(request, client)))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let config = Config::load("./config.toml", false);

    let proxy = Arc::new(Proxy { config });

    try_join!(
        https_listener_loop(proxy.clone()),
        http_listener_loop(proxy.clone())
    )?;
    Ok(())
}
