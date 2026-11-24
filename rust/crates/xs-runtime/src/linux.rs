use std::{io, mem::size_of, os::fd::RawFd, sync::OnceLock};

const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const LOAD_WORD: u16 = 0x20;
const EQUAL: u16 = 0x15;
const BIT_SET: u16 = 0x45;
const RETURN: u16 = 0x06;
const DENY: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;

const fn op(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

fn ensure_local_descriptors() -> io::Result<()> {
    for item in std::fs::read_dir("/proc/self/fd")? {
        let item = item?;
        let Some(fd) = item
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<RawFd>().ok())
        else {
            continue;
        };
        let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        // getsockname 只读取本进程描述符的套接字族。
        if unsafe {
            libc::getsockname(
                fd,
                (&mut address as *mut libc::sockaddr_storage).cast(),
                &mut len,
            )
        } == 0
            && i32::from(address.ss_family) != libc::AF_UNIX
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "启动时继承了非本地套接字",
            ));
        }
    }
    Ok(())
}

pub(crate) fn enforce() -> io::Result<()> {
    static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();
    INSTALLED
        .get_or_init(|| install().map_err(|error| error.to_string()))
        .clone()
        .map_err(io::Error::other)
}

fn install() -> io::Result<()> {
    ensure_local_descriptors()?;
    // seccomp_data: nr@0、arch@4、args[0]@16。x86_64 为小端，套接字族为 int。
    let mut filters = [
        op(LOAD_WORD, 4),
        jump(EQUAL, AUDIT_ARCH_X86_64, 1, 0),
        op(RETURN, DENY),
        op(LOAD_WORD, 0),
        jump(BIT_SET, X32_SYSCALL_BIT, 0, 1),
        op(RETURN, DENY),
        // io_uring 可以间接创建套接字，避免绕过系统调用过滤。
        jump(EQUAL, libc::SYS_io_uring_setup as u32, 0, 1),
        op(RETURN, DENY),
        jump(EQUAL, libc::SYS_socket as u32, 2, 0),
        jump(EQUAL, libc::SYS_socketpair as u32, 1, 0),
        op(RETURN, ALLOW),
        op(LOAD_WORD, 16),
        jump(EQUAL, libc::AF_UNIX as u32, 1, 0),
        op(RETURN, DENY),
        op(RETURN, ALLOW),
    ];
    let program = libc::sock_fprog {
        len: filters.len() as u16,
        filter: filters.as_mut_ptr(),
    };
    // 内核同步复制过滤器并让现有线程继承策略。
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC,
            &program,
        )
    };
    match result {
        0 => Ok(()),
        -1 => Err(io::Error::last_os_error()),
        _ => Err(io::Error::other("无法同步进程线程的离线策略")),
    }
}
