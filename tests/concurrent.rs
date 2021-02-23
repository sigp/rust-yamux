// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use futures::{channel::mpsc, prelude::*};
use std::{net::{Ipv4Addr, SocketAddr, SocketAddrV4}, sync::Arc};
use tokio::{net::TcpSocket, task, runtime::Runtime};
use tokio_util::compat::TokioAsyncReadCompatExt;
use yamux::{Config, Connection, Mode, WindowUpdateMode};
use quickcheck::{Arbitrary, Gen, QuickCheck};

async fn roundtrip(
    address: SocketAddr,
    nstreams: usize,
    data: Arc<Vec<u8>>,
    tcp_send_buffer_size: TcpSendBufferSize,
) {
    let listener = {
        let socket = TcpSocket::new_v4().expect("new_v4");
        if let Some(size) = tcp_send_buffer_size.0 {
            socket.set_send_buffer_size(size).expect("size set");
        }
        socket.bind(address).expect("bind");
        socket.listen(1024).expect("listen")
    };
    let address = listener.local_addr().expect("local address");

    let mut server_cfg = Config::default();
    // Use a large frame size to speed up the test.
    server_cfg.set_split_send_size(usize::min(256 * 1024, data.len()));
    // Use `WindowUpdateMode::OnRead` so window updates are sent by the
    // `Stream`s and subject to backpressure from the stream command channel. Thus
    // the `Connection` I/O loop will not need to send window updates
    // directly as a result of reading a frame, which can otherwise
    // lead to mutual write deadlocks if the socket send buffers are too small.
    // With `OnRead` the socket send buffer can even be smaller than the size
    // of a single frame for this test.
    server_cfg.set_window_update_mode(WindowUpdateMode::OnRead);
    let client_cfg = server_cfg.clone();

    let server = async move {
        let stream = listener.accept().await.expect("accept").0.compat();
        yamux::into_stream(Connection::new(stream, server_cfg, Mode::Server))
            .try_for_each_concurrent(None, |mut stream| async move {
                log::debug!("S: accepted new stream");
                let mut len = [0; 4];
                stream.read_exact(&mut len).await?;
                let mut buf = vec![0; u32::from_be_bytes(len) as usize];
                stream.read_exact(&mut buf).await?;
                stream.write_all(&buf).await?;
                stream.close().await?;
                Ok(())
            })
            .await
            .expect("server works")
    };

    task::spawn(server);

    let conn = {
        let socket = TcpSocket::new_v4().expect("new_v4");
        if let Some(size) = tcp_send_buffer_size.0 {
            socket.set_send_buffer_size(size).expect("size set");
        }
        let stream = socket.connect(address).await.expect("connect").compat();
        Connection::new(stream, client_cfg, Mode::Client)
    };
    let (tx, rx) = mpsc::unbounded();
    let mut ctrl = conn.control();
    task::spawn(yamux::into_stream(conn).for_each(|_| future::ready(())));
    for _ in 0 .. nstreams {
        let data = data.clone();
        let tx = tx.clone();
        let mut ctrl = ctrl.clone();
        task::spawn(async move {
            let mut stream = ctrl.open_stream().await?;
            log::debug!("C: opened new stream {}", stream.id());
            stream.write_all(&(data.len() as u32).to_be_bytes()[..]).await?;
            stream.write_all(&data).await?;
            stream.close().await?;
            log::debug!("C: {}: wrote {} bytes", stream.id(), data.len());
            let mut frame = vec![0; data.len()];
            stream.read_exact(&mut frame).await?;
            log::debug!("C: {}: read {} bytes", stream.id(), frame.len());
            assert_eq!(&data[..], &frame[..]);
            tx.unbounded_send(1).expect("unbounded_send");
            Ok::<(), yamux::ConnectionError>(())
        });
    }
    let n = rx.take(nstreams).fold(0, |acc, n| future::ready(acc + n)).await;
    ctrl.close().await.expect("close connection");
    assert_eq!(nstreams, n)
}

/// Send buffer size for a TCP socket.
///
/// Leave unchanged, i.e. use OS default value, when `None`.
#[derive(Clone, Debug)]
struct TcpSendBufferSize(Option<u32>);

impl Arbitrary for TcpSendBufferSize {
    fn arbitrary(g: &mut Gen) -> Self {
        if bool::arbitrary(g) {
            TcpSendBufferSize(None)
        } else {
            TcpSendBufferSize(Some(16 * 1024))
        }
    }
}


#[test]
fn concurrent_streams() {
    let _ = env_logger::try_init();

    fn prop (tcp_send_buffer_size: TcpSendBufferSize) {
        let data = Arc::new(vec![0x42; 128 * 1024]);
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));

        Runtime::new().expect("new runtime").block_on(roundtrip(addr, 1000, data, tcp_send_buffer_size));
    }

    QuickCheck::new().tests(1).quickcheck(prop as fn(_) -> _)
}
