extern crate socks;

use std::net::UdpSocket;
use std::net::SocketAddr;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::mpsc;
use std::thread;

use std::time::Duration;

use socks::Socks5Datagram;

const TIMEOUT: u64 = 30;

pub fn channel_to_socket(receiver: Receiver<(SocketAddr, Vec<u8>)>, socket: UdpSocket) {
    thread::spawn(move || {
        loop {
            let (dest, buf) = receiver.recv().unwrap();
            let to_send = buf.as_slice();
            socket
                .send_to(to_send, dest)
                .expect(&format!("Failed to forward response from upstream server to client {}",
                                 dest));
        }
    });
}

fn upstream_to_local(
    upstream_recv: Socks5Datagram,
    local_send_queue: Sender<(SocketAddr, Vec<u8>)>,
    src_addr: SocketAddr,
    local_timed_out: Arc<AtomicBool>)
{
    thread::spawn(move|| {
        let mut from_upstream = [0; 64 * 1024];
        upstream_recv.get_ref().set_read_timeout(Some(Duration::from_secs(TIMEOUT))).unwrap();
        loop {
            match upstream_recv.recv_from(&mut from_upstream) {
                Ok((bytes_rcvd, _)) => {
                    let to_send = from_upstream[..bytes_rcvd].to_vec();
                    local_send_queue.send((src_addr, to_send))
                        .expect("Failed to queue response from upstream server for forwarding!");
                },
                Err(_) => {
                    if local_timed_out.load(Ordering::Relaxed) {
                        break;
                    }
                }
            };
        }
    });
}

fn client_to_upstream(
    receiver: Receiver<Vec<u8>>,
    upstream_send: Socks5Datagram,
    timeouts: &mut u64,
    remote_addr: String,
    src_addr: SocketAddr,
    timed_out: Arc<AtomicBool>,
) {
    let remote_addr: &str = &remote_addr;
    loop {
        match receiver.recv_timeout(Duration::from_secs(TIMEOUT)) {
            Ok(from_client) => {
                upstream_send.send_to(from_client.as_slice(), remote_addr)
                    .expect(&format!("Failed to forward packet from client {} to upstream server!", src_addr));
                *timeouts = 0; //reset timeout count
            },
            Err(_) => {
                *timeouts += 1;
                if *timeouts >= 2 {
                    timed_out.store(true, Ordering::Relaxed);
                    break;
                }
            }
        };
    }
}

pub struct Forwarder {
    upstream_sender: Sender<Vec<u8>>,
}

impl Forwarder {
    pub fn new(
        local_send_queue: Sender<(SocketAddr, Vec<u8>)>,
        remote_addr: String,
        src_addr: SocketAddr,
        socks_addr: &str,
    ) -> Forwarder {
        let send_q = local_send_queue.clone();
        let remote_addr = remote_addr.clone();
        let sockaddrcopy0 = socks_addr.to_string();
        let (sender, receiver) = channel::<Vec<u8>>();
        thread::spawn(move|| {
            //regardless of which port we are listening to, we don't know which interface or IP
            //address the remote server is reachable via, so we bind the outgoing
            //connection to 0.0.0.0 in all cases.
            let temp_addr = format!("0.0.0.0:{}", 0);
            let socks5recv = Socks5Datagram::bind(sockaddrcopy0, temp_addr).expect("can't create socks 5 datagram endpoint");
            let socks5send = socks5recv.try_clone().unwrap();

            let mut timeouts : u64 = 0;
            let timed_out = Arc::new(AtomicBool::new(false));

            let local_timed_out = timed_out.clone();
            upstream_to_local(socks5recv,
                              send_q,
                              src_addr,
                              local_timed_out,
            );

            client_to_upstream(
                receiver,
                socks5send,
                & mut timeouts,
                remote_addr,
                src_addr, timed_out);

        });
        Forwarder {
            upstream_sender: sender
        }
    }

    pub fn send_upstream(&self, data: Vec<u8>) -> Result<(), mpsc::SendError<Vec<u8>>> {
        self.upstream_sender.send(data)
    }
}
