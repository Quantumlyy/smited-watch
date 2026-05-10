//! smited-watch — wrap a command, scan its output, fire haptic gRPC triggers.

pub mod cli;
pub mod client;
pub mod config;
pub mod debounce;
pub mod exit;
pub mod scan;
pub mod trigger;
pub mod wrap;

pub mod proto {
    pub mod smited {
        pub mod v1 {
            tonic::include_proto!("smited.v1");
        }
    }
}
