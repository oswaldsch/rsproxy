use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use config::Config;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper::{StatusCode, header};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

mod config;

struct Proxy {
    config: Config,
}

impl Proxy {
    pub async fn route(
        &self,
        request: Request<hyper::body::Incoming>,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        let Some(host) = request.headers().get("host") else {
            return Ok(make_response(
                "400 Bad Request: No Host header sent!",
                StatusCode::BAD_REQUEST,
            ));
        };
        let Ok(host) = host.to_str() else {
            return Ok(make_response(
                "400 Bad Request: Invalid Host header",
                StatusCode::BAD_REQUEST,
            ));
        };
        if self.config.hosts.contains_key(host) {
            return Ok(make_response(
                format!("Redirect to {}", self.config.hosts[host].forward),
                StatusCode::OK,
            ));
        } else {
            return Ok(make_response(
                format!("421 Misdirected Request: No route for host '{}'", host),
                StatusCode::MISDIRECTED_REQUEST,
            ));
        }
    }
}

fn make_response(body: impl Into<Bytes>, status_code: StatusCode) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body.into())));
    *response.status_mut() = status_code;
    let headers = response.headers_mut();
    // make the browser not interpret any html sent in the body because of user input
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    return response;
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
