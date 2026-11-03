mod support;

use std::convert::Infallible;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::Request,
    http::StatusCode,
    response::Response,
};
use http_body_util::BodyExt;
use rstest::rstest;
use support::{Gateway, client, recording, serve, within};
use tokio::sync::mpsc;

fn ok() -> Response {
    Response::new(Body::from("ok"))
}

#[rstest]
#[tokio::test]
async fn streams_upload_and_response_incrementally_without_a_body_limit() {
    let upstream = serve(
        Router::new().fallback(|request: Request| async { Response::new(request.into_body()) }),
    )
    .await;
    let gateway = Gateway::start(upstream).await;
    let (sender, receiver) = mpsc::channel::<Result<Bytes, Infallible>>(1);
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|chunk| (chunk, receiver))
    });
    sender.send(Ok(Bytes::from_static(b"first"))).await.unwrap();
    let mut response = within(
        client().request(
            Request::post(gateway.url("/v1/files"))
                .body(Body::from_stream(stream))
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    let frame = within(response.body_mut().frame()).await.unwrap().unwrap();
    assert_eq!(frame.into_data().unwrap(), "first");
    let large = Bytes::from(vec![b'x'; 8 * 1024 * 1024]);
    sender.send(Ok(large.clone())).await.unwrap();
    drop(sender);
    let rest = within(response.into_body().collect())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(rest, large);
}

#[rstest]
#[tokio::test]
async fn answers_expect_continue_so_large_uploads_start() {
    let (upstream, mut received) = recording(ok).await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send("POST /v1/audio/transcriptions HTTP/1.1\r\nhost: x\r\nexpect: 100-continue\r\ncontent-length: 4\r\n\r\n")
        .await;
    let interim = within(raw.response_head()).await.unwrap();
    assert_eq!(interim.status, StatusCode::CONTINUE);
    raw.send("data").await;
    assert_eq!(
        within(raw.response_head()).await.unwrap().status,
        StatusCode::OK
    );
    assert_eq!(received.recv().await.unwrap().body, "data");
}

#[rstest]
#[case::get("GET")]
#[case::delete("DELETE")]
#[case::options("OPTIONS")]
#[case::post("POST")]
#[tokio::test]
async fn forwards_a_chunked_body_on_any_method(#[case] method: &str) {
    let (upstream, mut received) = recording(ok).await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send(format!(
        "{method} /v1/search HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n4\r\nbody\r\n0\r\n\r\n"
    ))
    .await;
    assert_eq!(
        within(raw.response_head()).await.unwrap().status,
        StatusCode::OK
    );
    assert_eq!(received.recv().await.unwrap().body, "body");
}
