use crate::manager::Operation;
use std::{io, net::TcpListener, sync::mpsc, thread};
use tracing::info;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Server IO error. {0}")]
    Io(#[from] io::Error),

    #[error("Server channel disconnected")]
    ChannelDisconnected,
}

pub fn spawn(
    listener: TcpListener,
    op_tx: mpsc::SyncSender<Operation>,
) -> io::Result<thread::JoinHandle<Result<(), Error>>> {
    let builder = thread::Builder::new().name("server".into());
    builder.spawn(|| server_thread(listener, op_tx))
}

fn server_thread(listener: TcpListener, op_tx: mpsc::SyncSender<Operation>) -> Result<(), Error> {
    info!("Listening");
    loop {
        let (client, client_addr) = listener.accept()?;
        info!(peer = %client_addr, "Client connected");
        op_tx.send(Operation::HandleClient((client, client_addr)))?;
    }
}

impl<T> From<mpsc::SendError<T>> for Error {
    fn from(_value: mpsc::SendError<T>) -> Self {
        Error::ChannelDisconnected
    }
}
