use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};

use tauri::http::{
    header::{
        ACCEPT_RANGES, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
        ACCESS_CONTROL_ALLOW_ORIGIN, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE,
    },
    Method, Request, Response, StatusCode,
};

use crate::service::CoreService;

/// Keep a single WebView protocol response small even when the caller omits a
/// Range header or asks for the remainder of a multi-hour recording.
const MAX_ASSET_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

pub fn handle(service: &CoreService, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    if request.method() == Method::OPTIONS {
        return response(StatusCode::NO_CONTENT)
            .header(ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD, OPTIONS")
            .header(ACCESS_CONTROL_ALLOW_HEADERS, "Range")
            .body(Vec::new())
            .unwrap();
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let path = request.uri().path().trim_matches('/');
    let asset_id = path
        .strip_prefix("asset/")
        .or_else(|| (request.uri().host() == Some("asset") && !path.is_empty()).then_some(path))
        .unwrap_or_default();
    if asset_id.is_empty() || asset_id.contains('/') {
        return error(StatusCode::BAD_REQUEST, "asset id is missing");
    }
    let (path, descriptor) = match service.asset_stream_info(asset_id) {
        Ok(value) => value,
        Err(crate::error::CoreError::NotFound(_)) => {
            return error(StatusCode::NOT_FOUND, "asset not found");
        }
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid asset request"),
    };
    let total = descriptor.size_bytes;
    let requested_range = request
        .headers()
        .get(RANGE)
        .and_then(|value| value.to_str().ok());
    let range = match select_response_range(requested_range, total) {
        Ok(value) => value,
        Err(()) => {
            return response(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(CONTENT_RANGE, format!("bytes */{total}"))
                .header(ACCEPT_RANGES, "bytes")
                .body(Vec::new())
                .unwrap();
        }
    };
    let (start, end, status) = match range {
        Some((start, end)) => (start, end, StatusCode::PARTIAL_CONTENT),
        None => (0, 0, StatusCode::OK),
    };
    let length = if total == 0 {
        0
    } else {
        end.saturating_sub(start).saturating_add(1)
    };
    let mut builder = response(status)
        .header(CONTENT_TYPE, descriptor.content_type)
        .header(CONTENT_LENGTH, length.to_string())
        .header(ACCEPT_RANGES, "bytes")
        .header(ACCESS_CONTROL_ALLOW_ORIGIN, "*");
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(CONTENT_RANGE, format!("bytes {start}-{end}/{total}"));
    }
    builder
        .body(if request.method() == Method::HEAD {
            Vec::new()
        } else {
            let mut file = match File::open(&path) {
                Ok(file) => file,
                Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "asset read failed"),
            };
            if file.seek(SeekFrom::Start(start)).is_err() {
                return error(StatusCode::INTERNAL_SERVER_ERROR, "asset seek failed");
            }
            let Ok(length) = usize::try_from(length) else {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "asset range is too large",
                );
            };
            let mut bytes = vec![0_u8; length];
            if file.read_exact(&mut bytes).is_err() {
                return error(StatusCode::INTERNAL_SERVER_ERROR, "asset read failed");
            }
            bytes
        })
        .unwrap()
}

fn response(status: StatusCode) -> tauri::http::response::Builder {
    Response::builder().status(status)
}

fn error(status: StatusCode, message: &str) -> Response<Vec<u8>> {
    response(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(message.as_bytes().to_vec())
        .unwrap()
}

fn parse_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(header) = header else {
        return Ok(None);
    };
    if size == 0 {
        return Err(());
    }
    let value = header.strip_prefix("bytes=").ok_or(())?;
    if value.contains(',') {
        return Err(());
    }
    let (start, end) = value.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        let start = size.saturating_sub(suffix.min(size));
        return Ok(Some((start, size - 1)));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= size {
        return Err(());
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(size - 1)
    };
    if end < start {
        return Err(());
    }
    Ok(Some((start, end)))
}

fn select_response_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    if size == 0 {
        return match header {
            Some(_) => Err(()),
            None => Ok(None),
        };
    }

    let requested = parse_range(header, size)?;
    let (start, requested_end) = requested.unwrap_or((0, size - 1));
    let capped_end = start
        .saturating_add(MAX_ASSET_RESPONSE_BYTES - 1)
        .min(requested_end);
    Ok(Some((start, capped_end)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_byte_ranges() {
        assert_eq!(parse_range(None, 100), Ok(None));
        assert_eq!(parse_range(Some("bytes=10-19"), 100), Ok(Some((10, 19))));
        assert_eq!(parse_range(Some("bytes=90-"), 100), Ok(Some((90, 99))));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Ok(Some((90, 99))));
        assert_eq!(parse_range(Some("bytes=100-"), 100), Err(()));
        assert_eq!(parse_range(Some("bytes=1-2,4-5"), 100), Err(()));
    }

    #[test]
    fn caps_a_request_without_a_range_header() {
        let size = MAX_ASSET_RESPONSE_BYTES * 4;
        assert_eq!(
            select_response_range(None, size),
            Ok(Some((0, MAX_ASSET_RESPONSE_BYTES - 1)))
        );
    }

    #[test]
    fn caps_an_overbroad_range_from_its_requested_start() {
        let size = MAX_ASSET_RESPONSE_BYTES * 4;
        let start = 321;
        assert_eq!(
            select_response_range(Some(&format!("bytes={start}-")), size),
            Ok(Some((start, start + MAX_ASSET_RESPONSE_BYTES - 1)))
        );
    }

    #[test]
    fn preserves_small_ranges_and_rejects_invalid_ranges() {
        assert_eq!(
            select_response_range(Some("bytes=10-19"), 100),
            Ok(Some((10, 19)))
        );
        assert_eq!(select_response_range(Some("bytes=100-"), 100), Err(()));
        assert_eq!(select_response_range(Some("bytes=0-1,4-5"), 100), Err(()));
        assert_eq!(select_response_range(Some("bytes=0-"), 0), Err(()));
        assert_eq!(select_response_range(None, 0), Ok(None));
    }
}
