use super::*;
use std::{net::SocketAddr, os::fd::RawFd};
use tokio::io::AsyncReadExt;

// Inspect the outbound socket, not the server's unrelated accepted socket.
// The request is held before response headers while this descriptor is read.
fn outbound_tcp_user_timeout(local: SocketAddr, remote: SocketAddr) -> u32 {
    let address = |port| format!("0100007F:{port:04X}");
    let local = address(local.port());
    let remote = address(remote.port());
    let table = std::fs::read_to_string("/proc/self/net/tcp").unwrap();
    let inode = table
        .lines()
        .skip(1)
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|fields| fields[1] == local && fields[2] == remote)
        .unwrap()[9]
        .to_owned();
    let descriptor: RawFd = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            std::fs::read_link(entry.path())
                .is_ok_and(|target| target == std::path::Path::new(&format!("socket:[{inode}]")))
        })
        .unwrap()
        .file_name()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    unsafe extern "C" {
        fn getsockopt(fd: i32, level: i32, name: i32, value: *mut u32, length: *mut u32) -> i32;
    }
    let mut value = 0_u32;
    let mut length = std::mem::size_of_val(&value) as u32;
    // SAFETY: both pointers reference initialized, writable u32 values and the
    // Linux TCP_USER_TIMEOUT option writes one u32. No descriptor ownership is
    // taken. The in-flight client and accepted peer remain alive throughout.
    let result = unsafe { getsockopt(descriptor, 6, 18, &mut value, &mut length) };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    assert_eq!(length as usize, std::mem::size_of_val(&value));
    value
}

async fn read_headers(socket: &mut tokio::net::TcpStream) {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        headers.push(socket.read_u8().await.unwrap());
    }
}

#[tokio::test]
async fn actual_clients_inherit_os_tcp_recovery_and_cancel_unsent_responses() {
    let factory = Arc::new(HttpClientFactory::default());
    for purpose in HttpPurpose::ALL {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let source = factory.handle(purpose);
        let client = source
            .http_client(ClientOptions {
                no_retry: true,
                ..Default::default()
            })
            .unwrap();
        let send =
            tokio::spawn(async move { client.get(format!("http://{address}/")).send().await });
        let (mut socket, peer) = listener.accept().await.unwrap();
        read_headers(&mut socket).await;
        assert_eq!(outbound_tcp_user_timeout(peer, address), 0, "{purpose:?}");
        send.abort();
        assert!(send.await.unwrap_err().is_cancelled());
        assert_eq!(socket.read(&mut [0; 1]).await.unwrap(), 0, "{purpose:?}");
    }
}

#[tokio::test]
async fn explicit_600_second_request_deadline_survives_old_tcp_window_and_still_expires() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let source = HttpClient::default();
    let client = source
        .http_client(ClientOptions {
            no_retry: true,
            ..Default::default()
        })
        .unwrap();
    let send = tokio::spawn(async move {
        client
            .get(format!("http://{address}/"))
            .timeout(Duration::from_secs(600))
            .send()
            .await
    });
    let (mut socket, peer) = listener.accept().await.unwrap();
    read_headers(&mut socket).await;
    assert_eq!(outbound_tcp_user_timeout(peer, address), 0);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    assert!(!send.is_finished());
    tokio::time::advance(Duration::from_secs(568)).await;
    assert!(!send.is_finished());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(send.await.unwrap().unwrap_err().is_timeout());
    assert_eq!(socket.read(&mut [0; 1]).await.unwrap(), 0);
}
