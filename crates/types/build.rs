#![allow(missing_docs)]

use std::{env, io::Result};

fn main() -> Result<()> {
    let protos = &["proto/consensus.proto", "proto/sync.proto", "proto/liveness.proto"];

    for proto in protos {
        println!("cargo:rerun-if-changed={proto}");
    }

    if env::var("PROTOC").is_err() {
        let protoc = protoc_bin_vendored::protoc_bin_path().map_err(std::io::Error::other)?;
        unsafe {
            env::set_var("PROTOC", protoc);
        }
    }

    let mut config = prost_build::Config::new();
    config.enable_type_names();
    config.bytes(["."]);

    config.compile_protos(protos, &["proto"])?;

    Ok(())
}
