//! `devbox web` — start the local console.

use std::sync::Arc;

use anyhow::Result;
use clap::Args;

use crate::sandbox::SandboxManager;
use crate::web::{WebOptions, serve};

#[derive(Args, Debug)]
pub struct WebArgs {
    /// Port to listen on (the next free port is used if this one is taken)
    #[arg(long, default_value_t = crate::web::server::DEFAULT_PORT)]
    pub port: u16,

    /// Print the URL but do not open a browser
    #[arg(long)]
    pub no_open: bool,
}

pub async fn run(args: WebArgs, manager: &SandboxManager) -> Result<()> {
    // The server needs an owned handle it can share across tasks; the manager
    // is stateless beyond its state directory, so re-deriving one is cheap and
    // avoids threading lifetimes through every handler.
    let manager = Arc::new(SandboxManager {
        state_dir: manager.state_dir.clone(),
    });

    serve(
        manager,
        WebOptions {
            port: args.port,
            open: !args.no_open,
            landing: "/".to_string(),
        },
    )
    .await
}
