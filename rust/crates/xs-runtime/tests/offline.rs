#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::{
    io,
    net::{TcpListener, UdpSocket},
    os::unix::net::UnixStream,
    process::Command,
};

#[test]
fn tcp_udp_blocked_and_local_ipc_survives() {
    const CHILD: &str = "XUNSHU_OFFLINE_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert!(xs_runtime::enforce_offline().unwrap());
        for address in ["127.0.0.1:0", "[::1]:0"] {
            assert_eq!(
                TcpListener::bind(address).unwrap_err().raw_os_error(),
                Some(libc::EPERM)
            );
            assert_eq!(
                UdpSocket::bind(address).unwrap_err().raw_os_error(),
                Some(libc::EPERM)
            );
        }
        let (mut left, mut right) = UnixStream::pair().unwrap();
        io::Write::write_all(&mut left, b"x").unwrap();
        let mut bytes = [0];
        io::Read::read_exact(&mut right, &mut bytes).unwrap();
        assert_eq!(bytes, *b"x");
        return;
    }
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tcp_udp_blocked_and_local_ipc_survives"])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(status.success());
}
