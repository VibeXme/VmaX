//! Adapt RFC 8489 STUN and RFC 8656 ChannelData framing over TLS to the
//! datagram-style I/O interface used by the TURN client.
use bytes::{Buf, BytesMut};
use rustls::pki_types::ServerName;
use std::{io, net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf, split},
    net::TcpStream,
    sync::Mutex,
};
use tokio_rustls::{TlsConnector, client::TlsStream};
use webrtc_util::Conn;

const MAX_FRAME: usize = 4096;
type Stream = TlsStream<TcpStream>;

pub(super) struct TlsConn {
    local: SocketAddr,
    remote: SocketAddr,
    reader: Mutex<Reader>,
    writer: Mutex<WriteHalf<Stream>>,
}

struct Reader {
    stream: ReadHalf<Stream>,
    buffered: BytesMut,
}

impl TlsConn {
    pub(super) async fn connect(domain: &str, port: u16) -> io::Result<Self> {
        let root = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_root_certificates(root)
        .with_no_client_auth();
        Self::connect_with_config(domain, port, Arc::new(config)).await
    }

    async fn connect_with_config(
        domain: &str,
        port: u16,
        config: Arc<rustls::ClientConfig>,
    ) -> io::Result<Self> {
        let tcp = TcpStream::connect((domain, port)).await?;
        tcp.set_nodelay(true)?;
        let local = tcp.local_addr()?;
        let remote = tcp.peer_addr()?;
        let name = ServerName::try_from(domain.to_owned()).map_err(io::Error::other)?;
        let tls = TlsConnector::from(config).connect(name, tcp).await?;
        let (stream, writer) = split(tls);
        Ok(Self {
            local,
            remote,
            reader: Mutex::new(Reader {
                stream,
                buffered: BytesMut::with_capacity(MAX_FRAME),
            }),
            writer: Mutex::new(writer),
        })
    }
}

#[async_trait::async_trait]
impl Conn for TlsConn {
    async fn connect(&self, addr: SocketAddr) -> webrtc_util::Result<()> {
        if addr != self.remote {
            return Err(io::Error::other("TURN TLS peer changed").into());
        }
        Ok(())
    }

    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        let mut input = self.reader.lock().await;
        loop {
            if let Some((payload, wire)) = frame_length(&input.buffered)?
                && input.buffered.len() >= wire
            {
                if payload > buf.len() {
                    return Err(
                        io::Error::other("TURN TLS datagram exceeded receive buffer").into(),
                    );
                }
                buf[..payload].copy_from_slice(&input.buffered[..payload]);
                input.buffered.advance(wire);
                return Ok(payload);
            }
            let Reader { stream, buffered } = &mut *input;
            if stream.read_buf(buffered).await? == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "TURN TLS closed").into());
            }
        }
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        Ok((self.recv(buf).await?, self.remote))
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        let Some((payload, wire)) = frame_length(buf)? else {
            return Err(io::Error::other("Incomplete TURN TLS header").into());
        };
        if buf.len() != wire && buf.len() != payload {
            return Err(io::Error::other("Invalid TURN TLS datagram size").into());
        }
        let mut writer = self.writer.lock().await;
        writer.write_all(buf).await?;
        if buf.len() < wire {
            writer.write_all(&[0; 3][..wire - buf.len()]).await?;
        }
        writer.flush().await?;
        Ok(buf.len())
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        self.connect(target).await?;
        self.send(buf).await
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        Ok(self.local)
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.remote)
    }
    async fn close(&self) -> webrtc_util::Result<()> {
        self.writer.lock().await.shutdown().await?;
        Ok(())
    }
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

fn frame_length(bytes: &[u8]) -> io::Result<Option<(usize, usize)>> {
    if bytes.len() < 4 {
        return Ok(None);
    }
    let length = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
    let (payload, wire) = match bytes[0] >> 6 {
        0 if length % 4 == 0 => (20 + length, 20 + length),
        1 => (4 + length, (4 + length).next_multiple_of(4)),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid TURN TLS frame",
            ));
        }
    };
    if wire > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TURN TLS frame exceeded maximum length",
        ));
    }
    Ok(Some((payload, wire)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_stream_frames_distinguish_padding_from_payload() {
        assert_eq!(frame_length(&[0, 1, 0, 20]).unwrap(), Some((40, 40)));
        assert_eq!(frame_length(&[0x40, 0, 0, 1]).unwrap(), Some((5, 8)));
        assert_eq!(frame_length(&[0x40, 0, 0]).unwrap(), None);
        assert!(frame_length(&[0, 1, 0, 3]).is_err());
        assert!(frame_length(&[0x40, 0, 0xff, 0xff]).is_err());
    }

    #[tokio::test]
    async fn tls_preserves_stun_and_channel_data_boundaries() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let rcgen::CertifiedKey { cert, signing_key } =
                rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.der().clone()],
                    rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
                )
                .unwrap();
            let mut roots = rustls::RootCertStore::empty();
            roots.add(cert.der().clone()).unwrap();
            let client = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let channel = [0x40, 0, 0, 3, 1, 2, 3, 0];
            let mut binding = [0; 20];
            binding[1] = 1;
            binding[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes());
            let peer = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut tls = tokio_rustls::TlsAcceptor::from(Arc::new(config))
                    .accept(tcp)
                    .await
                    .unwrap();
                tls.write_all(&channel[..3]).await.unwrap();
                tls.flush().await.unwrap();
                tls.write_all(&[&channel[3..], &binding].concat())
                    .await
                    .unwrap();
                tls.flush().await.unwrap();
                let mut response = [0; 8];
                tls.read_exact(&mut response).await.unwrap();
                assert_eq!(response, channel);
            });
            let tls = TlsConn::connect_with_config("localhost", port, Arc::new(client))
                .await
                .unwrap();
            let mut buf = [0; 32];
            assert_eq!(tls.recv(&mut buf).await.unwrap(), 7);
            assert_eq!(&buf[..7], &channel[..7]);
            assert_eq!(tls.recv(&mut buf).await.unwrap(), 20);
            assert_eq!(&buf[..20], &binding);
            assert_eq!(tls.send(&channel[..7]).await.unwrap(), 7);
            peer.await.unwrap();
        })
        .await
        .expect("TURN TLS framing timed out");
    }
}
