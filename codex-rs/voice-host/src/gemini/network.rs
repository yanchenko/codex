//! Check actual UDP traffic before ICE can learn a peer-reflexive remote address.
use futures::future::BoxFuture;
use std::future::Future;
use std::io::IoSliceMut;
use std::io::{self};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use webrtc::runtime::AsyncInterval;
use webrtc::runtime::AsyncTcpListener;
use webrtc::runtime::AsyncTcpStream;
use webrtc::runtime::AsyncUdpSocket;
use webrtc::runtime::JoinHandle;
use webrtc::runtime::RecvMeta;
use webrtc::runtime::Runtime;
use webrtc::runtime::TokioRuntime;
use webrtc::runtime::Transmit;

#[derive(Debug, Default)]
pub(super) struct LocalRuntime;
fn allowed(address: SocketAddr) -> bool {
    address.port() != 0
        && super::peer::local_addresses().is_ok_and(|ips| ips.contains(&address.ip()))
}
fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "nonlocal voice media rejected",
    )
}
impl Runtime for LocalRuntime {
    fn spawn(&self, f: BoxFuture<'static, ()>) -> Box<dyn JoinHandle> {
        TokioRuntime.spawn(f)
    }
    fn spawn_reactor(&self, n: usize, f: BoxFuture<'static, ()>) -> Box<dyn JoinHandle> {
        TokioRuntime.spawn_reactor(n, f)
    }
    fn wrap_udp_socket(&self, s: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        if s.local_addr()?.ip() != std::net::Ipv4Addr::LOCALHOST {
            return Err(denied());
        }
        Ok(Arc::new(LocalSocket(TokioRuntime.wrap_udp_socket(s)?)))
    }
    // Gateway is UDP-only. Do not silently create a TCP path around the media guard.
    fn wrap_tcp_listener(&self, _: std::net::TcpListener) -> io::Result<Arc<dyn AsyncTcpListener>> {
        Err(denied())
    }
    fn connect_tcp(&self, _: SocketAddr) -> BoxFuture<'_, io::Result<Arc<dyn AsyncTcpStream>>> {
        Box::pin(async { Err(denied()) })
    }
    fn resolve_host<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<SocketAddr>>> {
        Box::pin(async move {
            let a: SocketAddr = host.parse().map_err(|_| denied())?;
            if allowed(a) {
                Ok(vec![a])
            } else {
                Err(denied())
            }
        })
    }
    fn sleep(&self, d: Duration) -> BoxFuture<'static, ()> {
        TokioRuntime.sleep(d)
    }
    fn interval(&self, d: Duration) -> Box<dyn AsyncInterval> {
        TokioRuntime.interval(d)
    }
    fn block_on(&self, f: Pin<Box<dyn Future<Output = ()> + '_>>) {
        TokioRuntime.block_on(f)
    }
    fn yield_now(&self) -> BoxFuture<'static, ()> {
        TokioRuntime.yield_now()
    }
    fn name(&self) -> &'static str {
        "local-voice"
    }
}
#[derive(Debug)]
struct LocalSocket(Arc<dyn AsyncUdpSocket>);
impl AsyncUdpSocket for LocalSocket {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
    fn poll_send(&self, cx: &mut Context<'_>, t: &Transmit<'_>) -> Poll<io::Result<usize>> {
        if !allowed(t.destination) {
            return Poll::Ready(Err(denied()));
        }
        self.0.poll_send(cx, t)
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        if bufs.is_empty() || meta.is_empty() {
            return Poll::Ready(Err(io::Error::other("empty receive buffers")));
        }
        for _ in 0..16 {
            match self.0.poll_recv(cx, &mut bufs[..1], &mut meta[..1]) {
                Poll::Ready(Ok(1)) if !allowed(meta[0].addr) => continue,
                other => return other,
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rejects_nonlocal_send_and_hostname_resolution() {
        assert!(!allowed("8.8.8.8:53".parse().unwrap()));
        assert!(LocalRuntime.resolve_host("localhost:80").await.is_err());
        assert!(
            LocalRuntime
                .connect_tcp("127.0.0.1:80".parse().unwrap())
                .await
                .is_err()
        );
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        let wrapped = LocalRuntime.wrap_udp_socket(sock).unwrap();
        assert!(
            wrapped
                .send_to(b"probe", "8.8.8.8:53".parse().unwrap())
                .await
                .is_err()
        );
    }
}
