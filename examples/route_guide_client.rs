//! Route Guide gRPC client example.
//!
//! Mirrors the canonical grpc-go route_guide client.
//! Exercises all four gRPC call types against the route_guide server.
//!
//! Run with (after starting route_guide_server):
//!   cargo run --example route_guide_client

use std::net::SocketAddr;

use prost::Message;

use grpc_rs::client::{Channel, StreamCall};
use grpc_rs::metadata::Metadata;

// ── Protobuf messages ─────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Message)]
struct Point {
    #[prost(int32, tag = "1")]
    latitude: i32,
    #[prost(int32, tag = "2")]
    longitude: i32,
}

#[derive(Clone, PartialEq, Message)]
struct Rectangle {
    #[prost(message, optional, tag = "1")]
    lo: Option<Point>,
    #[prost(message, optional, tag = "2")]
    hi: Option<Point>,
}

#[derive(Clone, PartialEq, Message)]
struct Feature {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(message, optional, tag = "2")]
    location: Option<Point>,
}

#[derive(Clone, PartialEq, Message)]
struct RouteNote {
    #[prost(message, optional, tag = "1")]
    location: Option<Point>,
    #[prost(string, tag = "2")]
    message: String,
}

#[derive(Clone, PartialEq, Message)]
struct RouteSummary {
    #[prost(int32, tag = "1")]
    point_count: i32,
    #[prost(int32, tag = "2")]
    feature_count: i32,
    #[prost(int32, tag = "3")]
    distance: i32,
    #[prost(int32, tag = "4")]
    elapsed_time: i32,
}

// ── RPC helpers ───────────────────────────────────────────────────────────────

async fn get_feature(ch: &Channel, lat: i32, lon: i32) {
    let req = Point { latitude: lat, longitude: lon };
    match ch.call_unary::<Point, Feature>("/routeguide.RouteGuide/GetFeature", &req, &Metadata::new(), None).await {
        Ok((f, _)) => {
            if f.name.is_empty() {
                println!("GetFeature({lat}, {lon}): no feature found");
            } else {
                println!("GetFeature({lat}, {lon}): {:?}", f.name);
            }
        }
        Err(e) => println!("GetFeature error: {e:?}"),
    }
}

async fn list_features(ch: &Channel) {
    let rect = Rectangle {
        lo: Some(Point { latitude: 400000000, longitude: -750000000 }),
        hi: Some(Point { latitude: 420000000, longitude: -730000000 }),
    };
    let mut call: StreamCall = ch
        .new_streaming_call("/routeguide.RouteGuide/ListFeatures", &Metadata::new(), None)
        .expect("open stream");
    call.send_message(&rect).expect("send rect");
    call.close_send().expect("close send");

    println!("ListFeatures:");
    while let Some(f) = call.recv_message::<Feature>().await.expect("recv") {
        println!("  - {:?} at {:?}", f.name, f.location);
    }
    call.finish().await.expect("finish");
}

async fn record_route(ch: &Channel) {
    let points = vec![
        Point { latitude: 412346009, longitude: -741496051 },
        Point { latitude: 433316622, longitude: -730620099 },
        Point { latitude: 400000000, longitude: -750000000 },
    ];

    let mut call: StreamCall = ch
        .new_streaming_call("/routeguide.RouteGuide/RecordRoute", &Metadata::new(), None)
        .expect("open stream");

    for p in &points {
        call.send_message(p).expect("send point");
    }
    call.close_send().expect("close send");

    let summary: RouteSummary = call.recv_message().await.expect("recv summary").expect("no summary");
    call.finish().await.expect("finish");
    println!(
        "RecordRoute: {} points, {} features, {}m, {}s",
        summary.point_count, summary.feature_count, summary.distance, summary.elapsed_time
    );
}

async fn route_chat(ch: &Channel) {
    let notes = vec![
        RouteNote { location: Some(Point { latitude: 0, longitude: 1 }), message: "First message".into() },
        RouteNote { location: Some(Point { latitude: 0, longitude: 2 }), message: "Second message".into() },
        RouteNote { location: Some(Point { latitude: 0, longitude: 1 }), message: "Third message".into() },
        RouteNote { location: Some(Point { latitude: 0, longitude: 2 }), message: "Fourth message".into() },
    ];

    let mut call: StreamCall = ch
        .new_streaming_call("/routeguide.RouteGuide/RouteChat", &Metadata::new(), None)
        .expect("open stream");

    println!("RouteChat:");
    for note in &notes {
        call.send_message(note).expect("send note");
        // Flush and read any echoed notes.
        // (A real client would use concurrent send/recv with separate tasks.)
    }
    call.close_send().expect("close send");

    while let Some(n) = call.recv_message::<RouteNote>().await.expect("recv note") {
        println!("  got note at {:?}: {:?}", n.location, n.message);
    }
    call.finish().await.expect("finish");
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let addr: SocketAddr = "127.0.0.1:50051".parse().unwrap();
    let ch = Channel::connect(addr).await.expect("connect");

    // Unary: look up a known and an unknown point.
    get_feature(&ch, 412346009, -741496051).await; // Berkshire Valley
    get_feature(&ch, 0, 0).await;                  // unknown

    // Server-streaming: list features in a bounding box.
    list_features(&ch).await;

    // Client-streaming: record a route.
    record_route(&ch).await;

    // Bidi-streaming: route chat.
    route_chat(&ch).await;
}
