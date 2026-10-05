//! Makes the server an iroh peer: devices sync programs with it, using the program's API key
//! as the token.

use std::sync::Arc;

use iroh::PublicKey;
use ribolita::MdbxStore;
use ribolita_node::{SharedEnv, net};

use crate::{api::AppState, programs};

pub(crate) struct ServerHost(pub(crate) Arc<AppState>);

impl net::Host for ServerHost {
    type Store = MdbxStore;

    fn env(&self, program: &str) -> Result<SharedEnv<MdbxStore>, String> {
        self.0.env(program).map_err(|err| err.to_string())
    }

    fn authorize(&self, program: &str, _: &PublicKey, token: Option<&str>) -> Result<(), String> {
        let expected = programs::load_api_key_for_program(&self.0.data_dir, program)
            .map_err(|err| err.to_string())?;
        match token {
            Some(token) if crate::api::constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
                Ok(())
            }
            _ => Err("a valid api key is required as the token".into()),
        }
    }
}

/// Starts the node and logs what it does.
pub(crate) fn spawn(state: Arc<AppState>, secret: iroh::SecretKey) {
    let (handle, events) = net::spawn(ServerHost(state.clone()), secret);
    state.node.set(handle.cmd_tx).ok();
    std::thread::spawn(move || {
        for event in events {
            match event {
                net::Event::PeerConnected(peer) => println!("peer connected: {peer}"),
                net::Event::PeerDisconnected(peer) => println!("peer disconnected: {peer}"),
                net::Event::Synced {
                    peer,
                    program,
                    received,
                    sent,
                    report,
                } => {
                    println!("synced '{program}' with {peer}: {received} in, {sent} out");
                    for (id, err) in report.failed {
                        println!("  entry {id} was rejected: {err}");
                    }
                }
                net::Event::Error(err) => eprintln!("sync: {err}"),
            }
        }
    });
}
