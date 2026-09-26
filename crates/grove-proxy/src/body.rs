use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};

/// Response body for everything the proxy sends: streamed upstream bodies
/// and the proxy's own pages.
pub(crate) type Body = BoxBody<Bytes, hyper::Error>;

pub(crate) fn full(data: impl Into<Bytes>) -> Body {
    Full::new(data.into())
        .map_err(|never| match never {})
        .boxed()
}

pub(crate) fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed()
}
