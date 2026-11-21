//! Fixed-size control messages on the first client-initiated bidirectional stream.
//! Fields are big-endian. The client writes HELLO before the host sends WELCOME.

use std::time::Duration;

use anyhow::{Result, ensure};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::time::timeout;

pub(super) const HELLO: u8 = 1;
pub(super) const WELCOME: u8 = 2;
pub(super) const PING: u8 = 3;
pub(super) const PONG: u8 = 4;
pub(super) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamSettings {
    pub width: u16,
    pub height: u16,
    pub fps: u16,
}

pub(super) async fn accept(connection: &Connection) -> Result<(SendStream, RecvStream)> {
    timeout(HANDSHAKE_TIMEOUT, async {
        let (send, mut recv) = connection.accept_bi().await?;
        send.set_priority(100)?;
        let mut hello = [0];
        recv.read_exact(&mut hello).await?;
        ensure!(hello == [HELLO], "invalid control stream hello");
        Ok((send, recv))
    })
    .await?
}

pub(super) async fn welcome(
    send: &mut SendStream,
    slot: usize,
    settings: StreamSettings,
) -> Result<()> {
    let [w0, w1] = settings.width.to_be_bytes();
    let [h0, h1] = settings.height.to_be_bytes();
    let [f0, f1] = settings.fps.to_be_bytes();
    timeout(
        HANDSHAKE_TIMEOUT,
        send.write_all(&[WELCOME, slot as u8, w0, w1, h0, h1, f0, f1]),
    )
    .await??;
    Ok(())
}

pub(super) async fn serve(mut send: SendStream, mut recv: RecvStream) -> Result<()> {
    loop {
        let mut heartbeat = [0; 9];
        timeout(HEARTBEAT_TIMEOUT, async {
            recv.read_exact(&mut heartbeat).await?;
            ensure!(heartbeat[0] == PING, "invalid heartbeat message");
            heartbeat[0] = PONG;
            send.write_all(&heartbeat).await?;
            Ok(())
        })
        .await??;
    }
}
