mod support;

use std::sync::Arc;

use axum::{body::Body, extract::Request, http::StatusCode};
use http_body_util::BodyExt;
use rstest::rstest;
use support::{Gateway, client, raw_upstream, within};
use tokio::sync::Notify;

fn chunk(bytes: &[u8]) -> Vec<u8> {
    [format!("{:x}\r\n", bytes.len()).as_bytes(), bytes, b"\r\n"].concat()
}

#[rstest]
#[tokio::test]
async fn each_event_reaches_the_client_before_python_produces_the_next() {
    let produce = Arc::new(Notify::new());
    let next = produce.clone();
    let upstream = raw_upstream(move |mut python| {
        let next = next.clone();
        async move {
            python.request_head().await.unwrap();
            python
                .send("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n")
                .await;
            for event in ["data: 1\n\n", "data: 2\n\n", "data: [DONE]\n\n"] {
                python.send(chunk(event.as_bytes())).await;
                next.notified().await;
            }
            python.send("0\r\n\r\n").await;
        }
    })
    .await;
    let gateway = Gateway::start(upstream).await;
    let mut response = within(
        client().request(
            Request::post(gateway.url("/v1/chat/completions"))
                .body(Body::from(r#"{"stream":true}"#))
                .unwrap(),
        ),
    )
    .await
    .unwrap();

    for event in ["data: 1\n\n", "data: 2\n\n", "data: [DONE]\n\n"] {
        let frame = within(response.body_mut().frame()).await.unwrap().unwrap();
        assert_eq!(frame.into_data().unwrap(), event);
        produce.notify_one();
    }
    assert!(within(response.body_mut().frame()).await.is_none());
}

#[rstest]
#[tokio::test]
async fn relays_a_binary_framed_stream_unchanged() {
    let message: Vec<u8> = [
        &[
            0x00, 0x00, 0x00, 0x3d, 0x00, 0x00, 0x00, 0x20, 0x8a, 0x0d, 0x0a, 0xff,
        ][..],
        b"\x0b:event-type\x07\x00\x05chunk\x0d:content-type\x07\x00\x10application/json",
        b"0\r\n\r\n{\"bytes\":\"eyJ0eXBlIjoiY29udGVudCJ9\"}",
        &[0xc3, 0x28, 0x00, 0x0d],
    ]
    .concat();
    let sent = message.clone();
    let upstream = raw_upstream(move |mut python| {
        let sent = sent.clone();
        async move {
            python.request_head().await.unwrap();
            python
                .send("HTTP/1.1 200 OK\r\ncontent-type: application/vnd.amazon.eventstream\r\ntransfer-encoding: chunked\r\n\r\n")
                .await;
            for piece in sent.chunks(7) {
                python.send(chunk(piece)).await;
            }
            python.send("0\r\n\r\n").await;
        }
    })
    .await;
    let gateway = Gateway::start(upstream).await;
    let response = within(
        client().request(
            Request::post(gateway.url("/bedrock/model/m/converse-stream"))
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(
        response.headers()["content-type"],
        "application/vnd.amazon.eventstream"
    );
    let body = within(response.into_body().collect())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(body, message);
}

#[rstest]
#[tokio::test]
async fn relays_a_body_delimited_by_the_upstream_closing() {
    let upstream = raw_upstream(|mut python| async move {
        python.request_head().await.unwrap();
        python
            .send("HTTP/1.1 200 OK\r\nconnection: close\r\n\r\nuntil eof")
            .await;
    })
    .await;
    let gateway = Gateway::start(upstream).await;
    let response = client().request(Request::get(gateway.url("/")).body(Body::empty()).unwrap());
    let body = within(response).await.unwrap().into_body().collect();
    assert_eq!(within(body).await.unwrap().to_bytes(), "until eof");
}

#[rstest]
#[tokio::test]
async fn an_upstream_dying_mid_stream_fails_the_client_body_instead_of_ending_it() {
    let upstream = raw_upstream(|mut python| async move {
        python.request_head().await.unwrap();
        python
            .send("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n6\r\ndata: \r\n")
            .await;
    })
    .await;
    let gateway = Gateway::start(upstream).await;
    let response =
        within(client().request(Request::get(gateway.url("/")).body(Body::empty()).unwrap()))
            .await
            .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(within(response.into_body().collect()).await.is_err());
}
