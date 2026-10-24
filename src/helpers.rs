use std::{fmt::Display, net::SocketAddr, str::FromStr, time::Duration};

use http_body_util::Full;
use hyper::{Method, Request, Response, StatusCode, Version, body::Bytes, header};

const HEADER_DELETE_LIST: [header::HeaderName; 11] = [
    header::HeaderName::from_static("keep-alive"),
    header::HeaderName::from_static("proxy-connection"),
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
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
        write!(
            formatter,
            "{} {} '{} {} {:.?}' {} -> {} {:.1?}",
            self.client,
            self.host.clone().unwrap_or("-".to_string()),
            self.method,
            self.path,
            self.version,
            self.status
                .expect("statuscode should always be set at this point"),
            self.upstream.clone().unwrap_or("-".to_string()),
            self.duration
                .expect("duration should always be set at this point")
        )
    }
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

pub fn clean_request<B>(
    mut request: Request<B>,
    replace_host: Option<String>,
) -> Result<Request<B>, header::InvalidHeaderValue> {
    let headers = request.headers_mut();
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
        headers.remove(header);
    }
    if let Some(new_host) = replace_host {
        headers.remove(header::HOST);
        headers.insert(header::HOST, header::HeaderValue::from_str(&new_host)?);
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use hyper::{HeaderMap, header::HeaderValue};

    use super::*;

    #[test]
    fn test_clean_request_delete_headers() {
        let mut req_builder = Request::builder();
        for header in HEADER_DELETE_LIST.iter() {
            req_builder = req_builder.header(header, HeaderValue::from_static("example value"));
        }
        let mut cleaned = clean_request(
            req_builder.body("test request to strip headers!").unwrap(),
            None,
        )
        .unwrap();
        let cleaned_headers = cleaned.headers_mut();
        for header in HEADER_DELETE_LIST.iter() {
            assert!(
                cleaned_headers.get(header).is_none(),
                "{header} was not stripped"
            )
        }
    }

    #[test]
    fn test_clean_request_delete_conn_headers() {
        let req = Request::builder()
            .header(header::CONNECTION, "X-Custom-Hop, , x-other ,")
            .header(header::CONNECTION, "x-second-hop")
            .header("x-custom-hop", "1")
            .header("x-other", "1")
            .header("x-second-hop", "1")
            .header(header::HOST, "example.com")
            .header("x-keep-me", "1")
            .body("test request to delete connection headers!")
            .unwrap();

        let cleaned = clean_request(req, None).unwrap();
        let h = cleaned.headers();

        for name in ["connection", "x-custom-hop", "x-other", "x-second-hop"] {
            assert!(!h.contains_key(name), "{name} was not stripped");
        }

        assert_eq!(h.get(header::HOST).unwrap(), "example.com");
        assert_eq!(h.get("x-keep-me").unwrap(), "1");
    }

    #[test]
    fn test_clean_request_preserve_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("example.com"));
        headers.insert("user-agent", HeaderValue::from_static("rsproxy-tester"));
        let mut req_builder = Request::builder();
        let headers_req = req_builder.headers_mut().unwrap();
        *headers_req = headers.clone();

        let cleaned_req = clean_request(
            req_builder
                .body("test request to preserve headers!")
                .unwrap(),
            None,
        )
        .unwrap();
        let cleaned_headers = cleaned_req.headers();

        assert_eq!(&headers, cleaned_headers)
    }
}
