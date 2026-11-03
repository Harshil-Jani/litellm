mod support;

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::{HeaderValue, StatusCode},
    response::Response,
};
use rstest::rstest;
use support::{Gateway, client, raw_upstream, recording, serve, within};

fn ok() -> Response {
    Response::new(Body::from("ok"))
}

#[rstest]
#[tokio::test]
async fn forwards_the_request_as_the_client_sent_it_minus_hop_headers() {
    let (upstream, mut received) = recording(|| {
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("connection", "x-upstream-hop")
            .header("x-upstream-hop", "remove")
            .header("x-response", "keep")
            .body(Body::from("python error"))
            .unwrap()
    })
    .await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send(
        "PATCH /v1/messages?x=a%2Fb&x=c HTTP/1.1\r\n\
         host: public.example\r\n\
         authorization: Bearer test\r\n\
         connection: x-remove, keep-alive\r\n\
         x-remove: remove\r\n\
         proxy-authorization: remove\r\n\
         te: trailers\r\n\
         content-length: 7\r\n\r\npayload",
    )
    .await;
    let head = within(raw.response_head()).await.unwrap();
    assert_eq!(head.status, StatusCode::UNAUTHORIZED);
    assert_eq!(head.header("x-response"), Some(&b"keep"[..]));
    assert_eq!(head.header("x-upstream-hop"), None);
    let body = raw.body(head.content_length().unwrap()).await;
    assert_eq!(body, b"python error");

    let request = received.recv().await.unwrap();
    assert_eq!(request.method, "PATCH");
    assert_eq!(request.uri, "/v1/messages?x=a%2Fb&x=c");
    assert_eq!(request.headers["host"], "public.example");
    assert_eq!(request.headers["authorization"], "Bearer test");
    assert!(!request.headers.contains_key("x-remove"));
    assert!(!request.headers.contains_key("proxy-authorization"));
    assert!(!request.headers.contains_key("te"));
    assert!(!request.headers.contains_key("connection"));
    assert_eq!(request.body, "payload");
}

#[rstest]
#[case::get("GET")]
#[case::post("POST")]
#[case::put("PUT")]
#[case::delete("DELETE")]
#[case::options("OPTIONS")]
#[case::extension("PURGE")]
#[tokio::test]
async fn forwards_every_method(#[case] method: &str) {
    let (upstream, mut received) = recording(ok).await;
    let gateway = Gateway::start(upstream).await;
    let response = client()
        .request(
            Request::builder()
                .method(method)
                .uri(gateway.url("/mcp"))
                .body(Body::from("body"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let request = received.recv().await.unwrap();
    assert_eq!(request.method, method);
    assert_eq!(request.body, "body");
}

#[rstest]
#[case::double_slash("//v1//models")]
#[case::encoded_slash("/v1/files/a%2Fb/content")]
#[case::encoded_query("/v1/models?q=%E2%9C%93&empty=&flag")]
#[case::utf8_bytes_escaped("/mcp/%F0%9F%A6%80")]
#[tokio::test]
async fn forwards_the_request_target_byte_for_byte(#[case] target: &str) {
    let (upstream, mut received) = recording(ok).await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send(format!("GET {target} HTTP/1.1\r\nhost: x\r\n\r\n"))
        .await;
    assert_eq!(
        within(raw.response_head()).await.unwrap().status,
        StatusCode::OK
    );
    assert_eq!(received.recv().await.unwrap().uri, target);
}

#[rstest]
#[tokio::test]
async fn absolute_form_targets_reach_python_as_origin_form() {
    let (upstream, mut received) = recording(ok).await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send(
        "GET http://elsewhere.example/v1/models?x=1 HTTP/1.1\r\nhost: elsewhere.example\r\n\r\n",
    )
    .await;
    assert_eq!(
        within(raw.response_head()).await.unwrap().status,
        StatusCode::OK
    );
    let request = received.recv().await.unwrap();
    assert_eq!(request.uri, "/v1/models?x=1");
}

#[rstest]
#[tokio::test]
async fn relays_a_response_with_more_headers_than_hyper_allows_by_default() {
    let upstream = serve(axum::Router::new().fallback(|| async {
        let headers = (0..150).map(|index| (format!("llm_provider-x-{index}"), "v"));
        let mut response = ok();
        for (name, value) in headers {
            response.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_static(value),
            );
        }
        response
    }))
    .await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;

    raw.send("GET /v1/chat/completions HTTP/1.1\r\nhost: public.example\r\n\r\n")
        .await;

    let head = within(raw.response_head()).await.unwrap();
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("llm_provider-x-149"), Some(&b"v"[..]));
}

#[rstest]
#[tokio::test]
async fn keeps_repeated_response_headers_and_opaque_header_bytes() {
    let (upstream, mut received) = recording(|| {
        Response::builder()
            .header("set-cookie", "a=1; Path=/")
            .header("set-cookie", "b=2; HttpOnly")
            .header("x-opaque", HeaderValue::from_bytes(b"caf\xe9").unwrap())
            .body(Body::empty())
            .unwrap()
    })
    .await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send(b"GET / HTTP/1.1\r\nhost: x\r\nx-opaque: caf\xe9\r\n\r\n")
        .await;
    let head = within(raw.response_head()).await.unwrap();
    let cookies: Vec<_> = head
        .headers
        .iter()
        .filter(|(name, _)| name == "set-cookie")
        .map(|(_, value)| value.as_slice())
        .collect();
    assert_eq!(cookies, [&b"a=1; Path=/"[..], b"b=2; HttpOnly"]);
    assert_eq!(head.header("x-opaque"), Some(&b"caf\xe9"[..]));
    assert_eq!(
        received.recv().await.unwrap().headers["x-opaque"].as_bytes(),
        b"caf\xe9"
    );
}

#[rstest]
#[tokio::test]
async fn answers_pipelined_requests_in_order() {
    let upstream = serve(
        Router::new().fallback(|request: Request| async move { request.uri().path().to_owned() }),
    )
    .await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send("GET /first HTTP/1.1\r\nhost: x\r\n\r\nHEAD /second HTTP/1.1\r\nhost: x\r\n\r\nGET /third HTTP/1.1\r\nhost: x\r\n\r\n")
        .await;
    let first = within(raw.response_head()).await.unwrap();
    assert_eq!(raw.body(first.content_length().unwrap()).await, b"/first");
    let second = within(raw.response_head()).await.unwrap();
    assert_eq!(second.content_length(), Some(7));
    let third = within(raw.response_head()).await.unwrap();
    assert_eq!(raw.body(third.content_length().unwrap()).await, b"/third");
}

#[rstest]
#[case::no_content(StatusCode::NO_CONTENT)]
#[case::not_modified(StatusCode::NOT_MODIFIED)]
#[tokio::test]
async fn bodiless_statuses_keep_the_connection_reusable(#[case] status: StatusCode) {
    let upstream = serve(Router::new().fallback(move |request: Request| async move {
        match request.uri().path() {
            "/next" => (StatusCode::OK, "next"),
            _ => (status, ""),
        }
    }))
    .await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send("GET /empty HTTP/1.1\r\nhost: x\r\n\r\nGET /next HTTP/1.1\r\nhost: x\r\n\r\n")
        .await;
    assert_eq!(within(raw.response_head()).await.unwrap().status, status);
    let next = within(raw.response_head()).await.unwrap();
    assert_eq!(raw.body(next.content_length().unwrap()).await, b"next");
}

#[rstest]
#[tokio::test]
async fn informational_responses_keep_the_connection_reusable() {
    let upstream = raw_upstream(|mut python| async move {
        while let Some(head) = python.request_head().await {
            match head.path.as_str() {
                "/hints" => {
                    python
                        .send("HTTP/1.1 103 Early Hints\r\nlink: </ui/app.css>; rel=preload\r\n\r\nHTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nfinal")
                        .await
                }
                _ => {
                    python
                        .send("HTTP/1.1 200 OK\r\ncontent-length: 4\r\n\r\nnext")
                        .await
                }
            }
        }
    })
    .await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send("GET /hints HTTP/1.1\r\nhost: x\r\n\r\nGET /next HTTP/1.1\r\nhost: x\r\n\r\n")
        .await;
    let mut hinted = within(raw.response_head()).await.unwrap();
    while hinted.status.is_informational() {
        hinted = within(raw.response_head()).await.unwrap();
    }
    assert_eq!(hinted.status, StatusCode::OK);
    assert_eq!(raw.body(hinted.content_length().unwrap()).await, b"final");
    let next = within(raw.response_head()).await.unwrap();
    assert_eq!(raw.body(next.content_length().unwrap()).await, b"next");
}

#[rstest]
#[case::found(
    StatusCode::FOUND,
    "https://idp.example/authorize?state=a%2Fb&redirect_uri=https%3A%2F%2Fpublic.example%2Fsso%2Fcallback"
)]
#[case::see_other(StatusCode::SEE_OTHER, "/ui/?login=success")]
#[case::temporary(StatusCode::TEMPORARY_REDIRECT, "./mcp/oauth/authorize?client_id=x")]
#[tokio::test]
async fn redirects_reach_the_client_with_location_and_cookies_unchanged(
    #[case] status: StatusCode,
    #[case] location: &'static str,
) {
    let (upstream, _received) = recording(move || {
        Response::builder()
            .status(status)
            .header("location", location)
            .header(
                "set-cookie",
                "token=a%2Fb; Path=/; HttpOnly; Secure; SameSite=Lax",
            )
            .header("set-cookie", "state=; Max-Age=0")
            .body(Body::empty())
            .unwrap()
    })
    .await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send("GET /sso/key/generate HTTP/1.1\r\nhost: public.example\r\n\r\n")
        .await;
    let head = within(raw.response_head()).await.unwrap();
    assert_eq!(head.status, status);
    assert_eq!(head.header("location"), Some(location.as_bytes()));
    let cookies: Vec<_> = head
        .headers
        .iter()
        .filter(|(name, _)| name == "set-cookie")
        .map(|(_, value)| value.as_slice())
        .collect();
    assert_eq!(
        cookies,
        [
            &b"token=a%2Fb; Path=/; HttpOnly; Secure; SameSite=Lax"[..],
            b"state=; Max-Age=0"
        ]
    );
}

#[rstest]
#[tokio::test]
async fn non_websocket_upgrades_reach_python_as_plain_requests() {
    let (upstream, mut received) = recording(ok).await;
    let gateway = Gateway::start(upstream).await;
    let mut raw = gateway.connect().await;
    raw.send("GET / HTTP/1.1\r\nhost: x\r\nconnection: upgrade, http2-settings\r\nupgrade: h2c\r\nhttp2-settings: AAMAAABkAAQAoAAAAAIAAAAA\r\n\r\n")
        .await;
    assert_eq!(
        within(raw.response_head()).await.unwrap().status,
        StatusCode::OK
    );
    let request = received.recv().await.unwrap();
    assert!(!request.headers.contains_key("upgrade"));
    assert!(!request.headers.contains_key("connection"));
    assert!(!request.headers.contains_key("http2-settings"));
}
