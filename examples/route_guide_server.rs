//! Route Guide gRPC server example.
//!
//! Mirrors the canonical grpc-go route_guide server.
//! Implements all four gRPC call types:
//!   GetFeature        — unary
//!   ListFeatures      — server-streaming
//!   RecordRoute       — client-streaming
//!   RouteChat         — bidirectional streaming
//!
//! Run with:
//!   cargo run --example route_guide_server

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use prost::Message;

use grpc_rs::codec;
use grpc_rs::metadata::Metadata;
use grpc_rs::server::{
    BoxFuture, Handler, MethodDesc, Server, ServerStream, ServiceDesc, StreamingHandlerFn,
    UnaryHandlerFn,
};
use grpc_rs::status::Status;

// ── Protobuf messages (mirrors routeguide/route_guide.proto) ─────────────────

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

// ── Static feature database ───────────────────────────────────────────────────

fn known_features() -> Vec<Feature> {
    vec![
        Feature { name: "Berkshire Valley Management Area Trail".into(), location: Some(Point { latitude: 412346009, longitude: -741496051 }) },
        Feature { name: "Merck Forest and Farmland Center".into(),        location: Some(Point { latitude: 433316622, longitude: -730620099 }) },
        Feature { name: "Morrisons Landing".into(),                       location: Some(Point { latitude: 406109563, longitude: -742186778 }) },
        Feature { name: "".into(),                                        location: Some(Point { latitude: 400000000, longitude: -750000000 }) },
    ]
}

fn find_feature<'a>(features: &'a [Feature], p: &Point) -> Option<&'a Feature> {
    features.iter().find(|f| {
        f.location.as_ref().map(|l| l.latitude == p.latitude && l.longitude == p.longitude).unwrap_or(false)
    })
}

fn in_range(p: &Point, rect: &Rectangle) -> bool {
    let (lo, hi) = match (&rect.lo, &rect.hi) {
        (Some(lo), Some(hi)) => (lo, hi),
        _ => return false,
    };
    let left   = lo.longitude.min(hi.longitude);
    let right  = lo.longitude.max(hi.longitude);
    let bottom = lo.latitude.min(hi.latitude);
    let top    = lo.latitude.max(hi.latitude);
    p.longitude >= left && p.longitude <= right && p.latitude >= bottom && p.latitude <= top
}

/// Haversine distance between two points in metres (integer).
fn calc_distance(p1: &Point, p2: &Point) -> i32 {
    const R: f64 = 6_371_000.0; // earth radius in metres
    let lat1 = (p1.latitude  as f64) * 1e-7_f64 * std::f64::consts::PI / 180.0;
    let lat2 = (p2.latitude  as f64) * 1e-7_f64 * std::f64::consts::PI / 180.0;
    let dlat = lat2 - lat1;
    let dlon = ((p2.longitude - p1.longitude) as f64) * 1e-7_f64 * std::f64::consts::PI / 180.0;
    let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    (R * c) as i32
}

// ── Handler constructors ──────────────────────────────────────────────────────

fn get_feature_handler(features: Arc<Vec<Feature>>) -> UnaryHandlerFn {
    Arc::new(move |req_bytes: Bytes, _md: Metadata| -> BoxFuture<Result<Bytes, Status>> {
        let features = Arc::clone(&features);
        Box::pin(async move {
            let point = Point::decode(req_bytes.as_ref())
                .map_err(|e| Status::internal(format!("decode: {e}")))?;
            let feature = find_feature(&features, &point)
                .cloned()
                .unwrap_or(Feature { name: String::new(), location: Some(point) });
            let mut buf = Vec::new();
            feature.encode(&mut buf).map_err(|e| Status::internal(format!("encode: {e}")))?;
            Ok(Bytes::from(buf))
        })
    })
}

fn list_features_handler(features: Arc<Vec<Feature>>) -> StreamingHandlerFn {
    Arc::new(move |mut stream: ServerStream, _md: Metadata| -> BoxFuture<()> {
        let features = Arc::clone(&features);
        Box::pin(async move {
            // Receive the Rectangle request.
            let req_frame = match stream.recv_message().await {
                Ok(Some(f)) => f,
                _ => { stream.send_trailers(&Status::internal("no request"), &http::HeaderMap::new()).ok(); return; }
            };
            let rect = match Rectangle::decode(&req_frame[5..]) {
                Ok(r) => r,
                Err(e) => { stream.send_trailers(&Status::internal(format!("decode: {e}")), &http::HeaderMap::new()).ok(); return; }
            };

            stream.send_headers(&http::HeaderMap::new()).ok();

            for feature in features.iter() {
                if let Some(loc) = &feature.location {
                    if in_range(loc, &rect) {
                        let mut buf = Vec::new();
                        if feature.encode(&mut buf).is_ok() {
                            let frame = codec::encode_raw(&buf, None).unwrap();
                            if stream.send_message(frame).is_err() { break; }
                        }
                    }
                }
            }
            stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
        })
    })
}

fn record_route_handler(features: Arc<Vec<Feature>>) -> StreamingHandlerFn {
    Arc::new(move |mut stream: ServerStream, _md: Metadata| -> BoxFuture<()> {
        let features = Arc::clone(&features);
        Box::pin(async move {
            let start = Instant::now();
            let mut point_count = 0i32;
            let mut feature_count = 0i32;
            let mut distance = 0i32;
            let mut last_point: Option<Point> = None;

            loop {
                match stream.recv_message().await {
                    Ok(Some(frame)) => {
                        if let Ok(p) = Point::decode(&frame[5..]) {
                            point_count += 1;
                            if find_feature(&features, &p).map(|f| !f.name.is_empty()).unwrap_or(false) {
                                feature_count += 1;
                            }
                            if let Some(prev) = &last_point {
                                distance += calc_distance(prev, &p);
                            }
                            last_point = Some(p);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }

            let elapsed = start.elapsed().as_secs() as i32;
            let summary = RouteSummary { point_count, feature_count, distance, elapsed_time: elapsed };
            let mut buf = Vec::new();
            if summary.encode(&mut buf).is_ok() {
                stream.send_headers(&http::HeaderMap::new()).ok();
                let frame = codec::encode_raw(&buf, None).unwrap();
                stream.send_message(frame).ok();
            }
            stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
        })
    })
}

fn route_chat_handler() -> StreamingHandlerFn {
    Arc::new(move |mut stream: ServerStream, _md: Metadata| -> BoxFuture<()> {
        Box::pin(async move {
            // Accumulate all notes received, echo back any previously received note
            // at the same location (matches grpc-go route_guide behavior).
            let mut received: Vec<RouteNote> = Vec::new();

            stream.send_headers(&http::HeaderMap::new()).ok();

            loop {
                match stream.recv_message().await {
                    Ok(Some(frame)) => {
                        if let Ok(note) = RouteNote::decode(&frame[5..]) {
                            // Echo back all previous notes at this location.
                            for prev in received.iter().filter(|n| {
                                n.location == note.location
                            }) {
                                let mut buf = Vec::new();
                                if prev.encode(&mut buf).is_ok() {
                                    let f = codec::encode_raw(&buf, None).unwrap();
                                    if stream.send_message(f).is_err() { break; }
                                }
                            }
                            received.push(note);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
        })
    })
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let addr: SocketAddr = "0.0.0.0:50051".parse().unwrap();
    let features = Arc::new(known_features());

    let mut server = Server::new();
    server.add_service(ServiceDesc {
        name: "routeguide.RouteGuide",
        methods: vec![
            MethodDesc { name: "GetFeature",   handler: Handler::Unary(get_feature_handler(Arc::clone(&features))) },
            MethodDesc { name: "ListFeatures", handler: Handler::Streaming(list_features_handler(Arc::clone(&features))) },
            MethodDesc { name: "RecordRoute",  handler: Handler::Streaming(record_route_handler(Arc::clone(&features))) },
            MethodDesc { name: "RouteChat",    handler: Handler::Streaming(route_chat_handler()) },
        ],
    });

    println!("Route guide server listening on {addr}");
    server.serve(addr).await.expect("server failed");
}
