//! A route that deliberately panics must still return a clean 500 and the
//! server must keep serving afterwards. Exercises the exact
//! `catch_panic_layer()` the real app uses, on a minimal router so no
//! production route needs a deliberate panic path.

mod common;

use axum::{Router, routing::get};
use mediaqueue::middleware::catch_panic_layer;

async fn boom() -> &'static str {
    panic!("deliberate test panic")
}

async fn ok() -> &'static str {
    "still here"
}

#[tokio::test]
async fn panicking_route_returns_500_and_server_keeps_serving() {
    let router = Router::new()
        .route("/boom", get(boom))
        .route("/ok", get(ok))
        .layer(catch_panic_layer());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random port");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("test server crashed");
    });

    let panicking = common::http_request(addr, "GET", "/boom", &[], None).await;
    assert_eq!(panicking.status, 500, "body was: {}", panicking.body);
    assert!(panicking.body.contains("\"error\":\"internal_error\""));

    // The server process (and this listener) must still be serving.
    let ok = common::http_request(addr, "GET", "/ok", &[], None).await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.body, "still here");
}
