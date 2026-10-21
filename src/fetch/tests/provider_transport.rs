use super::*;

struct EarlyEofBody {
    sent: bool,
}

impl hyper::body::Body for EarlyEofBody {
    type Data = hyper::body::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        if self.sent {
            std::task::Poll::Ready(None)
        } else {
            self.sent = true;
            std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(
                hyper::body::Bytes::from_static(b"abc"),
            ))))
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        hyper::body::SizeHint::with_exact(if self.sent { 2 } else { 5 })
    }
}

#[tokio::test]
async fn decoder_rejects_eof_with_declared_bytes_still_missing() {
    let response = Response::from(
        http::Response::builder()
            .header(CONTENT_TYPE, "text/html")
            .header("Content-Length", "5")
            .body(reqwest::Body::wrap(EarlyEofBody { sent: false }))
            .unwrap(),
    );
    assert_eq!(response.content_length(), Some(5));
    let encoding = response_encoding(&response).unwrap();
    let error = fetcher(Limits::default())
        .read_html(response, encoding, &RequestContext::new(BUDGET))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::UpstreamFailed);
    assert_eq!(error.message, "Upstream closed an incomplete HTML response");
}
