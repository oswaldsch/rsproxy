use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use config::Config;
use helpers::make_plain_response;
use http_body_util::{BodyExt, Full};
use hyper::StatusCode;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
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
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        let Some(host) = request.headers().get("host") else {
            return Ok(make_plain_response(
                "400 Bad Request: No Host header sent!",
                StatusCode::BAD_REQUEST,
            ));
        };
        let Ok(host) = host.to_str() else {
            return Ok(make_plain_response(
                "400 Bad Request: Invalid Host header",
                StatusCode::BAD_REQUEST,
            ));
        };
        if self.config.hosts.contains_key(host) {
            let owned_host = host.to_string();
            let response = self.forward_request(request, owned_host).await;
            if response.is_ok() {
                let (response_head, response_body) = response.unwrap().into_parts();
                if let Ok(response_body) = response_body.collect().await {
                    // FIXME: proxy loads entire response into ram instead of streaming
                    let response_body = Full::new(response_body.to_bytes());
                    let response = Response::from_parts(response_head, response_body);
                    Ok(response)
                } else {
                    return Ok(make_plain_response(
                        "502 Bad Gateway",
                        StatusCode::BAD_GATEWAY,
                    ));
                }
            } else {
                return Ok(make_plain_response(
                    "502 Bad Gateway",
                    StatusCode::BAD_GATEWAY,
                ));
            }
        } else {
            return Ok(make_plain_response(
                format!("421 Misdirected Request: No route for host '{}'", host),
                StatusCode::MISDIRECTED_REQUEST,
            ));
        }
    }

    async fn forward_request(
        &self,
        request: Request<hyper::body::Incoming>,
        host: String,
    ) -> Result<Response<Incoming>, Box<dyn std::error::Error + Send + Sync>> {
        let stream = TcpStream::connect(self.config.hosts[&host].forward.clone()).await?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                println!("Connection failed: {:?}", err);
            }
        });

        let response = sender.send_request(request).await?;
        Ok(response)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = Config::load("./config.toml").unwrap_or_else(|err| {
        eprintln!("failed to load config: {}", err);
        std::process::exit(1)
    });

    let proxy = Arc::new(Proxy { config });

    let addr = SocketAddr::from(([127, 0, 0, 1], proxy.config.listen));
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, _) = listener.accept().await?;

        let io = TokioIo::new(stream);
        let proxy = Arc::clone(&proxy);

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(|request| proxy.route(request)))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}
