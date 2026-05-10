//! smited-watch — wrap a command, scan its output, fire haptic gRPC triggers.

pub mod config;

pub mod proto {
    pub mod smited {
        pub mod v1 {
            tonic::include_proto!("smited.v1");
        }
    }
}
