use crate::manager::Operation;
use crossbeam_channel::Sender;
use std::{io, net::TcpListener, thread};
use tracing::info;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Server IO error. {0}")]
    Io(#[from] io::Error),
}

pub fn spawn(
    listener: TcpListener,
    op_tx: Sender<Operation>,
) -> io::Result<thread::JoinHandle<Result<(), Error>>> {
    let builder = thread::Builder::new().name("server".into());
    builder.spawn(|| server_thread(listener, op_tx))
}

fn server_thread(listener: TcpListener, op_tx: Sender<Operation>) -> Result<(), Error> {
    info!("Listening");
    loop {
        let (client, client_addr) = listener.accept()?;

        // Forward the new client to the manager
        info!(peer = %client_addr, "Client connected");
        if op_tx
            .send(Operation::HandleClient((client, client_addr)))
            .is_err()
        {
            break;
        }
    }
    info!("Shutting down");
    Ok(())
}
