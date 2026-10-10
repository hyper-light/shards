//! BuildKit's frontend gateway (frontend/gateway/pb/gateway.proto, LLBBridge) as a
//! frontend speaks it (D113): gRPC over HTTP/2 over the pipe BuildKit gives a frontend's
//! process, its standard input and output, as grpcclient.RunFromEnvironment dials it.
//! A client of one connection, its calls one at a time, every frame and message bounded.

pub mod gateway;
pub mod grpc;
pub mod h2;
pub mod hpack;
mod hpack_tables;
pub mod wire;
