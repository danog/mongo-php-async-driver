//! The socket calls the driver makes beyond std, per platform: readiness polling (poll(2) /
//! WSAPoll), the non-blocking connect of server checks, the closed-peer check, and the raw
//! socket handle. The rest of the code uses only these and std.

use std::io;
use std::net::{SocketAddr, TcpStream};

pub use imp::*;

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};

    /// A socket handle.
    pub type RawSock = std::os::fd::RawFd;
    /// One entry of a poll set (poll(2)'s struct pollfd).
    pub type PollFd = libc::pollfd;

    pub const POLLIN: i16 = libc::POLLIN;
    pub const POLLOUT: i16 = libc::POLLOUT;
    pub const POLLERR: i16 = libc::POLLERR;
    pub const POLLHUP: i16 = libc::POLLHUP;

    /// A poll-set entry for `fd`.
    pub fn pollfd(fd: RawSock, events: i16) -> PollFd {
        libc::pollfd { fd, events, revents: 0 }
    }

    /// The socket of a stream.
    pub fn raw(s: &TcpStream) -> RawSock {
        s.as_raw_fd()
    }

    /// Drops the stream without closing its socket (it belongs to another process after fork).
    pub fn forget(s: TcpStream) {
        let _ = s.into_raw_fd();
    }

    /// One poll(2) call (`ms`: -1 waits without limit): the count of ready entries.
    pub fn poll(fds: &mut [PollFd], ms: i32) -> io::Result<usize> {
        // SAFETY: a valid, exclusively borrowed pollfd array of the given length
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// A new non-blocking TCP socket (std's defaults: close-on-exec, no SIGPIPE) and a connect
    /// to `sa` started on it: Ok(true) connected, Ok(false) in progress, Err the connect's error.
    pub fn connect_start(sa: &SocketAddr) -> io::Result<(TcpStream, io::Result<bool>)> {
        let family = if sa.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
        // SAFETY: plain socket creation; the descriptor is owned by the TcpStream right away
        let fd = unsafe { libc::socket(family, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let s = unsafe { TcpStream::from_raw_fd(fd) };
        socket_defaults(fd);
        s.set_nonblocking(true)?;
        let (st, len) = sockaddr(sa);
        // SAFETY: a sockaddr of the given length
        let r = if unsafe { libc::connect(fd, (&st as *const libc::sockaddr_storage).cast(), len) } == 0 {
            Ok(true)
        } else {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINPROGRESS) {
                Ok(false)
            } else {
                Err(e)
            }
        };
        Ok((s, r))
    }

    /// mongoc_socket_check_closed: readable without data, or a socket error. One poll(2), plus a
    /// MSG_PEEK recv when readable.
    pub fn check_closed(s: &TcpStream) -> bool {
        let fd = s.as_raw_fd();
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd; a zero timeout
        if unsafe { libc::poll(&mut p, 1, 0) } <= 0 || p.revents == 0 {
            return false;
        }
        let mut b = 0u8;
        // SAFETY: a one-byte buffer we own; MSG_PEEK leaves the data in the socket
        let r = unsafe { libc::recv(fd, (&mut b as *mut u8).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
        r < 1
    }

    /// What std sets on its own sockets: close-on-exec, and (Apple) no SIGPIPE on writes to a
    /// closed peer.
    fn socket_defaults(fd: libc::c_int) {
        // SAFETY: fcntl/setsockopt on a descriptor the caller owns
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            #[cfg(target_vendor = "apple")]
            {
                let one: libc::c_int = 1;
                libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, (&one as *const libc::c_int).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t);
            }
        }
    }

    /// The C socket address of `sa`.
    fn sockaddr(sa: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
        // SAFETY: an all-zero sockaddr_storage is valid; the casts write within its size
        let mut st: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let len = match sa {
            SocketAddr::V4(a) => {
                let p = unsafe { &mut *(&mut st as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
                p.sin_family = libc::AF_INET as libc::sa_family_t;
                p.sin_port = a.port().to_be();
                p.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) };
                #[cfg(any(target_vendor = "apple", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
                {
                    p.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
                }
                std::mem::size_of::<libc::sockaddr_in>()
            }
            SocketAddr::V6(a) => {
                let p = unsafe { &mut *(&mut st as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>() };
                p.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                p.sin6_port = a.port().to_be();
                p.sin6_addr = libc::in6_addr { s6_addr: a.ip().octets() };
                p.sin6_flowinfo = a.flowinfo();
                p.sin6_scope_id = a.scope_id();
                #[cfg(any(target_vendor = "apple", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
                {
                    p.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
                }
                std::mem::size_of::<libc::sockaddr_in6>()
            }
        };
        (st, len as libc::socklen_t)
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket};
    use windows_sys::Win32::Networking::WinSock as ws;

    /// A socket handle (SOCKET).
    pub type RawSock = ws::SOCKET;
    /// One entry of a poll set (WSAPoll's WSAPOLLFD).
    pub type PollFd = ws::WSAPOLLFD;

    pub const POLLIN: i16 = ws::POLLIN;
    pub const POLLOUT: i16 = ws::POLLOUT;
    pub const POLLERR: i16 = ws::POLLERR;
    pub const POLLHUP: i16 = ws::POLLHUP;

    /// A poll-set entry for `fd`.
    pub fn pollfd(fd: RawSock, events: i16) -> PollFd {
        ws::WSAPOLLFD { fd, events, revents: 0 }
    }

    /// The socket of a stream.
    pub fn raw(s: &TcpStream) -> RawSock {
        s.as_raw_socket() as RawSock
    }

    /// Drops the stream without closing its socket.
    pub fn forget(s: TcpStream) {
        let _ = s.into_raw_socket();
    }

    fn last_error() -> io::Error {
        // SAFETY: no arguments
        io::Error::from_raw_os_error(unsafe { ws::WSAGetLastError() })
    }

    /// WSAStartup, once (std calls it before its own first socket call; a socket made here may
    /// come first). Never cleaned up, as std's.
    fn startup() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // SAFETY: a zeroed WSADATA out-parameter
            let mut data: ws::WSADATA = unsafe { std::mem::zeroed() };
            unsafe { ws::WSAStartup(0x202, &mut data) };
        });
    }

    /// One WSAPoll call (`ms`: -1 waits without limit): the count of ready entries. An empty set
    /// (which WSAPoll refuses) sleeps for `ms`.
    pub fn poll(fds: &mut [PollFd], ms: i32) -> io::Result<usize> {
        if fds.is_empty() {
            if ms >= 0 {
                std::thread::sleep(std::time::Duration::from_millis(ms as u64));
            }
            return Ok(0);
        }
        // SAFETY: a valid, exclusively borrowed WSAPOLLFD array of the given length
        let n = unsafe { ws::WSAPoll(fds.as_mut_ptr(), fds.len() as u32, ms) };
        if n < 0 {
            return Err(last_error());
        }
        Ok(n as usize)
    }

    /// A new non-blocking TCP socket (std's defaults: overlapped, not inherited by child
    /// processes) and a connect to `sa` started on it: Ok(true) connected, Ok(false) in progress,
    /// Err the connect's error.
    pub fn connect_start(sa: &SocketAddr) -> io::Result<(TcpStream, io::Result<bool>)> {
        startup();
        let family = if sa.is_ipv4() { ws::AF_INET } else { ws::AF_INET6 };
        // SAFETY: plain socket creation; the handle is owned by the TcpStream right away
        let s = unsafe { ws::WSASocketW(i32::from(family), ws::SOCK_STREAM, ws::IPPROTO_TCP, std::ptr::null(), 0, ws::WSA_FLAG_OVERLAPPED | ws::WSA_FLAG_NO_HANDLE_INHERIT) };
        if s == ws::INVALID_SOCKET {
            return Err(last_error());
        }
        let stream = unsafe { TcpStream::from_raw_socket(s as u64) };
        stream.set_nonblocking(true)?;
        let (st, len) = sockaddr(sa);
        // SAFETY: a socket address of the given length
        let r = if unsafe { ws::connect(s, (&st as *const ws::SOCKADDR_STORAGE).cast(), len) } == 0 {
            Ok(true)
        } else {
            let e = last_error();
            if e.raw_os_error() == Some(ws::WSAEWOULDBLOCK) {
                Ok(false)
            } else {
                Err(e)
            }
        };
        Ok((stream, r))
    }

    /// mongoc_socket_check_closed: readable without data, or a socket error. One WSAPoll, plus a
    /// MSG_PEEK recv when readable (it returns at once: the socket is readable).
    pub fn check_closed(s: &TcpStream) -> bool {
        let fd = raw(s);
        let mut p = [pollfd(fd, POLLIN)];
        // SAFETY: one valid WSAPOLLFD; a zero timeout
        if unsafe { ws::WSAPoll(p.as_mut_ptr(), 1, 0) } <= 0 || p[0].revents == 0 {
            return false;
        }
        if p[0].revents & POLLIN == 0 {
            // POLLERR / POLLHUP only
            return true;
        }
        let mut b = 0u8;
        // SAFETY: a one-byte buffer we own; MSG_PEEK leaves the data in the socket
        let r = unsafe { ws::recv(fd, &mut b, 1, ws::MSG_PEEK) };
        r < 1
    }

    /// mongoc_winerr_to_string: "(0x<code, 8 hex digits>) <the system's message>".
    pub fn error_text(code: u32) -> String {
        use windows_sys::Win32::System::Diagnostics::Debug::{FormatMessageA, FORMAT_MESSAGE_ALLOCATE_BUFFER, FORMAT_MESSAGE_ARGUMENT_ARRAY, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS};
        let mut msg: *mut u8 = std::ptr::null_mut();
        // MAKELANGID(LANG_NEUTRAL, SUBLANG_DEFAULT)
        const LANG: u32 = 0x400;
        // SAFETY: with FORMAT_MESSAGE_ALLOCATE_BUFFER the buffer argument receives a LocalAlloc'd
        // NUL-terminated string (freed below)
        let n = unsafe { FormatMessageA(FORMAT_MESSAGE_ALLOCATE_BUFFER | FORMAT_MESSAGE_ARGUMENT_ARRAY | FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS, std::ptr::null(), code, LANG, (&mut msg as *mut *mut u8).cast(), 0, std::ptr::null()) };
        if n == 0 || msg.is_null() {
            if !msg.is_null() {
                unsafe { windows_sys::Win32::Foundation::LocalFree(msg.cast()) };
            }
            return format!("(0x{code:08X}) (Failed to get error message)");
        }
        // SAFETY: n bytes written by FormatMessageA
        let bytes = unsafe { std::slice::from_raw_parts(msg, n as usize) };
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        unsafe { windows_sys::Win32::Foundation::LocalFree(msg.cast()) };
        // the trailing "\r\n" or "\n" goes
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        format!("(0x{code:08X}) {text}")
    }

    /// The socket address of `sa`.
    fn sockaddr(sa: &SocketAddr) -> (ws::SOCKADDR_STORAGE, i32) {
        // SAFETY: an all-zero SOCKADDR_STORAGE is valid; the casts write within its size
        let mut st: ws::SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
        let len = match sa {
            SocketAddr::V4(a) => {
                let p = unsafe { &mut *(&mut st as *mut ws::SOCKADDR_STORAGE).cast::<ws::SOCKADDR_IN>() };
                p.sin_family = ws::AF_INET;
                p.sin_port = a.port().to_be();
                p.sin_addr.S_un.S_addr = u32::from_ne_bytes(a.ip().octets());
                std::mem::size_of::<ws::SOCKADDR_IN>()
            }
            SocketAddr::V6(a) => {
                let p = unsafe { &mut *(&mut st as *mut ws::SOCKADDR_STORAGE).cast::<ws::SOCKADDR_IN6>() };
                p.sin6_family = ws::AF_INET6;
                p.sin6_port = a.port().to_be();
                p.sin6_addr.u.Byte = a.ip().octets();
                p.sin6_flowinfo = a.flowinfo();
                p.Anonymous.sin6_scope_id = a.scope_id();
                std::mem::size_of::<ws::SOCKADDR_IN6>()
            }
        };
        (st, len as i32)
    }
}
