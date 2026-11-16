//! BuildKit's frontend gateway (frontend/gateway/pb/gateway.proto, LLBBridge) as a
//! frontend speaks it (D113): gRPC over HTTP/2 over the pipe BuildKit gives a frontend's
//! process, its standard input and output, as grpcclient.RunFromEnvironment dials it.
//! A client of one connection, its calls one at a time, every frame and message bounded.

pub mod gateway;
pub mod grpc;
pub mod h2;
/// HTTP/2's field compression (RFC 7541), whose fields a response carries: shards-http2's,
/// the credential injector's too (R12).
pub use shards_http2::hpack;
pub mod wire;
