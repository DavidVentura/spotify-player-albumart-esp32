use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::CStr;
use core::fmt::Write;

const MAX_HEADERS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    Accounts,
    Api,
    ImageCdn,
}

impl Host {
    pub const fn name(self) -> &'static CStr {
        match self {
            Host::Accounts => c"accounts.spotify.com",
            Host::Api => c"api.spotify.com",
            Host::ImageCdn => c"i.scdn.co",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Authorization {
    Anonymous,
    Basic(String),
    Bearer(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Body {
    Empty,
    Form(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub host: Host,
    pub path: String,
    pub authorization: Authorization,
    pub body: Body,
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let method = match self.method {
            Method::Get => "GET",
            Method::Post => "POST",
        };
        let host = self
            .host
            .name()
            .to_str()
            .expect("host names are ASCII literals");

        let mut head = String::new();
        write!(head, "{method} {} HTTP/1.1\r\n", self.path).unwrap();
        write!(head, "Host: {host}\r\n").unwrap();
        // Spotify's CDN gzips when asked; never ask, so bodies arrive as-is
        head.push_str("Accept-Encoding: identity\r\n");
        head.push_str("Connection: close\r\n");
        match &self.authorization {
            Authorization::Anonymous => {}
            Authorization::Basic(credentials) => {
                write!(head, "Authorization: Basic {credentials}\r\n").unwrap()
            }
            Authorization::Bearer(token) => {
                write!(head, "Authorization: Bearer {token}\r\n").unwrap()
            }
        }
        let body = match &self.body {
            Body::Empty => "",
            Body::Form(form) => {
                head.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
                write!(head, "Content-Length: {}\r\n", form.len()).unwrap();
                form.as_str()
            }
        };
        head.push_str("\r\n");
        head.push_str(body);
        head.into_bytes()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(pub u16);

impl Status {
    pub const OK: Status = Status(200);
    pub const NO_CONTENT: Status = Status(204);
    pub const UNAUTHORIZED: Status = Status(401);

    fn has_body(self) -> bool {
        !(self.0 < 200 || self == Status::NO_CONTENT || self.0 == 304)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Response {
    pub status: Status,
    pub body: Vec<u8>,
}

/// Whether the peer may still send bytes after the ones already received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Open,
    Closed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parse {
    Complete(Response),
    NeedMore,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HttpError {
    Malformed(httparse::Error),
    InvalidContentLength,
    DuplicateFramingHeader,
    ConflictingFraming,
    UnsupportedTransferEncoding,
    InvalidChunk,
    TrailingData,
    Truncated,
}

enum Framing {
    Empty,
    Length(usize),
    Chunked,
    UntilClose,
}

/// Parses everything received so far on a connection. Call again with more
/// bytes appended while it returns `NeedMore`.
pub fn parse_response(raw: &[u8], stream: Stream) -> Result<Parse, HttpError> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_len = match parsed.parse(raw).map_err(HttpError::Malformed)? {
        httparse::Status::Partial => return incomplete(stream),
        httparse::Status::Complete(len) => len,
    };
    let status = Status(parsed.code.expect("complete response has a status code"));
    let rest = &raw[header_len..];

    let body = match framing(status, parsed.headers)? {
        Framing::Empty if rest.is_empty() => Some(Vec::new()),
        Framing::Empty => return Err(HttpError::TrailingData),
        Framing::Length(len) if rest.len() > len => return Err(HttpError::TrailingData),
        Framing::Length(len) if rest.len() == len => Some(rest.to_vec()),
        Framing::Length(_) => None,
        Framing::Chunked => decode_chunked(rest)?,
        Framing::UntilClose => match stream {
            Stream::Open => None,
            Stream::Closed => Some(rest.to_vec()),
        },
    };
    match body {
        Some(body) => Ok(Parse::Complete(Response { status, body })),
        None => incomplete(stream),
    }
}

fn incomplete(stream: Stream) -> Result<Parse, HttpError> {
    match stream {
        Stream::Open => Ok(Parse::NeedMore),
        Stream::Closed => Err(HttpError::Truncated),
    }
}

fn framing(status: Status, headers: &[httparse::Header]) -> Result<Framing, HttpError> {
    let mut content_length = None;
    let mut transfer_encoding = None;
    for header in headers {
        let slot = if header.name.eq_ignore_ascii_case("content-length") {
            &mut content_length
        } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
            &mut transfer_encoding
        } else {
            continue;
        };
        if slot.replace(header.value).is_some() {
            return Err(HttpError::DuplicateFramingHeader);
        }
    }

    if !status.has_body() {
        return Ok(Framing::Empty);
    }
    match (transfer_encoding, content_length) {
        (Some(_), Some(_)) => Err(HttpError::ConflictingFraming),
        (Some(encoding), None) if encoding.trim_ascii().eq_ignore_ascii_case(b"chunked") => {
            Ok(Framing::Chunked)
        }
        (Some(_), None) => Err(HttpError::UnsupportedTransferEncoding),
        (None, Some(length)) => core::str::from_utf8(length.trim_ascii())
            .ok()
            .and_then(|length| length.parse().ok())
            .map(Framing::Length)
            .ok_or(HttpError::InvalidContentLength),
        (None, None) => Ok(Framing::UntilClose),
    }
}

/// Returns `None` until the terminating zero-size chunk and its trailer
/// section have been received.
fn decode_chunked(data: &[u8]) -> Result<Option<Vec<u8>>, HttpError> {
    let mut body = Vec::new();
    let mut pos = 0;
    loop {
        let (size_line_len, size) = match httparse::parse_chunk_size(&data[pos..]) {
            Ok(httparse::Status::Complete(parsed)) => parsed,
            Ok(httparse::Status::Partial) => return Ok(None),
            Err(httparse::InvalidChunkSize) => return Err(HttpError::InvalidChunk),
        };
        pos += size_line_len;

        if size == 0 {
            return skip_trailers(&data[pos..]).map(|complete| complete.then_some(body));
        }

        let start = pos;
        let end = usize::try_from(size)
            .ok()
            .and_then(|size| start.checked_add(size))
            .ok_or(HttpError::InvalidChunk)?;
        let Some(after_chunk) = data.get(end..) else {
            return Ok(None);
        };
        if after_chunk.len() < 2 {
            return Ok(None);
        }
        if &after_chunk[..2] != b"\r\n" {
            return Err(HttpError::InvalidChunk);
        }
        body.extend_from_slice(&data[start..end]);
        pos = end + 2;
    }
}

fn skip_trailers(data: &[u8]) -> Result<bool, HttpError> {
    let mut pos = 0;
    loop {
        let Some(line_len) = data[pos..].windows(2).position(|w| w == b"\r\n") else {
            return Ok(false);
        };
        pos += line_len + 2;
        if line_len == 0 {
            break;
        }
    }
    if pos != data.len() {
        return Err(HttpError::TrailingData);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn complete(raw: &[u8], stream: Stream) -> Response {
        match parse_response(raw, stream) {
            Ok(Parse::Complete(response)) => response,
            other => panic!("expected a complete response, got {other:?}"),
        }
    }

    #[test]
    fn encodes_form_post() {
        let request = Request {
            method: Method::Post,
            host: Host::Accounts,
            path: "/api/token".to_string(),
            authorization: Authorization::Basic("Y2xpZW50OnNlY3JldA==".to_string()),
            body: Body::Form("grant_type=refresh_token".to_string()),
        };
        let expected = "POST /api/token HTTP/1.1\r\n\
                        Host: accounts.spotify.com\r\n\
                        Accept-Encoding: identity\r\n\
                        Connection: close\r\n\
                        Authorization: Basic Y2xpZW50OnNlY3JldA==\r\n\
                        Content-Type: application/x-www-form-urlencoded\r\n\
                        Content-Length: 24\r\n\
                        \r\n\
                        grant_type=refresh_token";
        assert_eq!(request.encode(), expected.as_bytes());
    }

    #[test]
    fn encodes_anonymous_get_without_body_headers() {
        let request = Request {
            method: Method::Get,
            host: Host::ImageCdn,
            path: "/image/abc".to_string(),
            authorization: Authorization::Anonymous,
            body: Body::Empty,
        };
        let expected = "GET /image/abc HTTP/1.1\r\n\
                        Host: i.scdn.co\r\n\
                        Accept-Encoding: identity\r\n\
                        Connection: close\r\n\
                        \r\n";
        assert_eq!(request.encode(), expected.as_bytes());
    }

    #[test]
    fn content_length_body_waits_for_all_bytes() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        for cut in 0..raw.len() {
            assert_eq!(
                parse_response(&raw[..cut], Stream::Open),
                Ok(Parse::NeedMore)
            );
        }
        let response = complete(raw, Stream::Open);
        assert_eq!(response.status, Status::OK);
        assert_eq!(response.body, b"hello");
    }

    #[test]
    fn content_length_body_truncated_by_close() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel";
        assert_eq!(
            parse_response(raw, Stream::Closed),
            Err(HttpError::Truncated)
        );
    }

    #[test]
    fn content_length_rejects_extra_bytes() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhello";
        assert_eq!(
            parse_response(raw, Stream::Open),
            Err(HttpError::TrailingData)
        );
    }

    #[test]
    fn decodes_chunked_body_across_partial_reads() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nX-Trailer: 1\r\n\r\n";
        for cut in 0..raw.len() {
            assert_eq!(
                parse_response(&raw[..cut], Stream::Open),
                Ok(Parse::NeedMore)
            );
        }
        assert_eq!(complete(raw, Stream::Open).body, b"hello world");
    }

    #[test]
    fn rejects_chunk_without_crlf_terminator() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhiXX0\r\n\r\n";
        assert_eq!(
            parse_response(raw, Stream::Open),
            Err(HttpError::InvalidChunk)
        );
    }

    #[test]
    fn body_until_close() {
        let raw = b"HTTP/1.1 200 OK\r\n\r\nabc";
        assert_eq!(parse_response(raw, Stream::Open), Ok(Parse::NeedMore));
        assert_eq!(complete(raw, Stream::Closed).body, b"abc");
    }

    #[test]
    fn no_content_completes_without_body() {
        let raw = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
        let response = complete(raw, Stream::Open);
        assert_eq!(response.status, Status::NO_CONTENT);
        assert!(response.body.is_empty());
    }

    #[test]
    fn rejects_both_framings() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(
            parse_response(raw, Stream::Open),
            Err(HttpError::ConflictingFraming)
        );
    }

    #[test]
    fn rejects_compressed_transfer_encoding() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n";
        assert_eq!(
            parse_response(raw, Stream::Open),
            Err(HttpError::UnsupportedTransferEncoding)
        );
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-LENGTH: 2\r\n\r\nok";
        assert_eq!(complete(raw, Stream::Open).body, b"ok");
    }
}
