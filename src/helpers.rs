use std::{
    fmt::{Display, Formatter, write},
    net::SocketAddr,
    str::FromStr,
    time::{Duration, Instant},
};

use http_body_util::{Empty, Full};
use hyper::{Method, Request, Response, StatusCode, Version, body::Bytes, header, server::conn};
use log::{info, warn};

use crate::helpers::InvalidAuthority::{EmptyLabel, Malformed, PortGiven};

const HEADER_DELETE_LIST: [header::HeaderName; 10] = [
    header::HeaderName::from_static("keep-alive"),
    header::HeaderName::from_static("proxy-connection"),
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::FORWARDED,
    header::HeaderName::from_static("x-real-ip"),
    header::CONNECTION,
];

pub struct LogEntry {
    pub client: SocketAddr,
    pub host: Option<String>,
    pub method: Method,
    pub path: String,
    pub version: Version,
    pub status: Option<StatusCode>,
    pub upstream: Option<String>,
    pub duration: Option<Duration>,
}

impl Display for LogEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        let status_color = match self.status.unwrap().as_u16() {
            100..=199 => "\x1b[2m",
            200..=299 => "\x1b[32m",
            300..=399 => "\x1b[36m",
            400..=499 => "\x1b[33m",
            500..=599 => "\x1b[31m",
            _ => "",
        };
        let reset = "\x1b[0m";
        write!(
            formatter,
            "{} {} '{} {} {:.?}' {}{}{} -> {} {:.1?}",
            self.client,
            self.host.as_deref().unwrap_or("-"),
            self.method,
            self.path,
            self.version,
            status_color,
            self.status
                .expect("statuscode should always be set at this point"),
            reset,
            self.upstream.as_deref().unwrap_or("-"),
            self.duration
                .expect("duration should always be set at this point")
        )
    }
}

#[derive(Debug)]
pub enum InvalidAuthority {
    Malformed(http::uri::InvalidUri),
    EmptyLabel(String),
    PortGiven(String),
}

impl Display for InvalidAuthority {
    fn fmt(&self, formatter: &mut Formatter) -> Result<(), std::fmt::Error> {
        match self {
            Malformed(err) => write!(formatter, "malformed authority: {}", err),
            EmptyLabel(authority) => write!(formatter, "empty label in {}", authority),
            PortGiven(authority) => write!(formatter, "forward contains a port '{}'", authority),
        }
    }
}

pub fn check_via_header(
    request: &Request<hyper::body::Incoming>,
    origin: SocketAddr,
) -> Option<Response<Full<Bytes>>> {
    let headers = request.headers();
    let via_headers = headers.get_all("Via");
    for via_header in via_headers {
        let via_value = via_header.as_bytes();
        if via_value
            .split(|byte| byte == &b' ')
            .any(|value| value == "rsproxy".as_bytes())
        // TODO: make this configurable to comply with spec
        {
            warn!("proxy loop detected coming from {}", origin);
            let mut response = make_plain_response(
                "502 Bad Gateway: Proxy loop detected",
                StatusCode::BAD_GATEWAY,
            );
            let headers = response.headers_mut();
            headers.append(
                header::HeaderName::from_static("proxy-status"),
                header::HeaderValue::from_static("rsproxy; error=proxy_loop_detected"),
            );
            return Some(response);
        }
    }

    None
}

pub fn check_upgrade_header(request: &Request<hyper::body::Incoming>) -> bool {
    let headers = request.headers();
    if headers.get(header::UPGRADE).is_some() {
        let conn_headers = headers.get_all(header::CONNECTION);
        for conn_header in conn_headers {
            if conn_header
                .as_bytes()
                .split(|c| c == &b',')
                .any(|s| s.to_ascii_lowercase().trim_ascii() == b"upgrade")
            {
                return true;
            }
        }
    }
    false
}

pub fn make_plain_response(
    body: impl Into<Bytes>,
    status_code: StatusCode,
) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(body.into()));
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
    response
}

pub fn clean_headers(
    headers: &mut header::HeaderMap<header::HeaderValue>,
    replace_host: Option<String>,
    upgrade_flag: bool,
) -> Result<(), header::InvalidHeaderValue> {
    let conn_headers: Vec<header::HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|conn_header| conn_header.to_str().ok())
        .flat_map(|header_str| {
            header_str
                .split(',')
                .map(|conn_header_str| conn_header_str.trim())
        })
        .filter_map(|conn_header| header::HeaderName::from_str(conn_header).ok())
        .collect();
    for header in HEADER_DELETE_LIST.iter().chain(conn_headers.iter()) {
        if upgrade_flag && header == header::UPGRADE {
            continue;
        } else {
            headers.remove(header);
        }
    }
    if let Some(new_host) = replace_host {
        headers.remove(header::HOST);
        headers.insert(header::HOST, header::HeaderValue::from_str(&new_host)?);
    }
    if upgrade_flag {
        headers.insert(
            header::CONNECTION,
            header::HeaderValue::from_static("upgrade"),
        );
    }
    Ok(())
}

pub fn canonical_host(authority: &str, parsing_config: bool) -> Result<String, InvalidAuthority> {
    let parsed_authority = http::uri::Authority::from_str(authority);
    let parsed_authority = parsed_authority.map_err(InvalidAuthority::Malformed)?;
    let host = parsed_authority.host();
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.split('.').any(|s| s.is_empty()) {
        return Err(InvalidAuthority::EmptyLabel(host.to_string()));
    }
    if parsed_authority.port_u16().is_some() && parsing_config {
        return Err(InvalidAuthority::PortGiven(authority.to_string()));
    }
    Ok(host.to_ascii_lowercase())
}

pub fn handle_host(
    request: &Request<hyper::body::Incoming>,
    log_entry: &mut LogEntry,
    incoming_ts: Instant,
) -> (Option<Response<Full<Bytes>>>, Option<String>) {
    let Some(host) = request.headers().get("host") else {
        log_entry.duration = Some(incoming_ts.elapsed());
        log_entry.status = Some(StatusCode::BAD_REQUEST);
        info!("no Host header sent");
        info!("{}", log_entry);
        return (
            Some(make_plain_response(
                "400 Bad Request: No Host header sent!",
                StatusCode::BAD_REQUEST,
            )),
            None,
        );
    };
    let Ok(host) = host.to_str() else {
        log_entry.duration = Some(incoming_ts.elapsed());
        log_entry.status = Some(StatusCode::BAD_REQUEST);
        info!("Host header is not an ascii string");
        info!("{}", log_entry);
        return (
            Some(make_plain_response(
                "400 Bad Request: Invalid Host header",
                StatusCode::BAD_REQUEST,
            )),
            None,
        );
    };
    let host = canonical_host(host, false);
    if let Err(err) = host {
        log_entry.duration = Some(incoming_ts.elapsed());
        log_entry.status = Some(StatusCode::BAD_REQUEST);
        info!("Host header invalid: {}", err);
        info!("{}", log_entry);
        return (
            Some(make_plain_response(
                "400 Bad Request: Invalid Host header",
                StatusCode::BAD_REQUEST,
            )),
            None,
        );
    }
    (None, Some(host.unwrap()))
}

pub fn get_version_literal(version: Version) -> Option<String> {
    match version {
        Version::HTTP_09 => Some("0.9".to_string()),
        Version::HTTP_10 => Some("1.0".to_string()),
        Version::HTTP_11 => Some("1.1".to_string()),
        Version::HTTP_2 => Some("2".to_string()),
        Version::HTTP_3 => Some("3".to_string()),
        _ => None,
    }
}
