mod support;

use axum::{body::Body, http::StatusCode, response::Response};
use litellm_gateway_forward::PEER_HEADER;
use rstest::rstest;
use support::{Gateway, recording, within};

fn ok() -> Response {
    Response::new(Body::from("ok"))
}

#[rstest]
#[tokio::test]
async fn python_sees_forwarding_headers_untouched_and_the_tcp_peer_rust_saw() {
    let (upstream, mut received) = recording(ok).await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send(
        "GET /sso/callback HTTP/1.1\r\n\
         host: public.example\r\n\
         x-forwarded-for: 192.0.2.1\r\n\
         x-forwarded-for: 192.0.2.2\r\n\
         x-forwarded-proto: https\r\n\
         forwarded: for=192.0.2.1;proto=https\r\n\
         x-litellm-rust-peer: 10.1.2.3:1\r\n\r\n",
    )
    .await;
    assert_eq!(
        within(raw.response_head()).await.unwrap().status,
        StatusCode::OK
    );

    let request = received.recv().await.unwrap();
    let forwarded_for: Vec<_> = request.headers.get_all("x-forwarded-for").iter().collect();
    assert_eq!(forwarded_for, ["192.0.2.1", "192.0.2.2"]);
    assert_eq!(request.headers["x-forwarded-proto"], "https");
    assert_eq!(request.headers["forwarded"], "for=192.0.2.1;proto=https");
    let peers: Vec<_> = request.headers.get_all(PEER_HEADER).iter().collect();
    assert_eq!(peers, [raw.local_address().to_string().as_str()]);
}

#[rstest]
#[tokio::test]
async fn http2_clients_reach_python_with_their_public_host() {
    let (upstream, mut received) = recording(ok).await;
    let gateway = Gateway::start(upstream).await;
    let stream = tokio::net::TcpStream::connect(gateway.address)
        .await
        .unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(stream),
    )
    .await
    .unwrap();
    tokio::spawn(connection);
    let request = axum::http::Request::get("http://public.example/v1/models")
        .body(Body::empty())
        .unwrap();

    let response = within(sender.send_request(request)).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        received.recv().await.unwrap().headers["host"],
        "public.example"
    );
}
