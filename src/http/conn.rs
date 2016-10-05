use std::borrow::Cow;
use std::fmt;
use std::hash::Hash;
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::time::Duration;

use futures::{Poll, Async};
use tokio::io::FramedIo;

use http::{self, h1, Http1Transaction, Io, WriteBuf};
//use http::channel;
use http::buffer::Buffer;
use net::{Transport};
use version::HttpVersion;


/// This handles a connection, which will have been established over a
/// Transport (like a socket), and will likely include multiple
/// `Transaction`s over HTTP.
///
/// The connection will determine when a message begins and ends, creating
/// a new message `TransactionHandler` for each one, as well as determine if this
/// connection can be kept alive after the message, or if it is complete.
pub struct Conn<K: Key, T: Transport, H: ConnectionHandler<T>> {
    handler: H,
    io: Io<T>,
    keep_alive_enabled: bool,
    key: K,
    state: State<H::Txn, T>,
}

impl<K: Key, T: Transport, H: ConnectionHandler<T>> Conn<K, T, H> {
    pub fn new(key: K, transport: T, handler: H) -> Conn<K, T, H> {
        Conn {
            handler: handler,
            io: Io {
                buf: Buffer::new(),
                transport: transport,
            },
            keep_alive_enabled: true,
            key: key,
            state: State::Init {
            },
        }
    }

    fn parse(&mut self) -> ::Result<Option<http::MessageHead<<<<H as ConnectionHandler<T>>::Txn as TransactionHandler<T>>::Transaction as Http1Transaction>::Incoming>>> {
        self.io.parse::<<<H as ConnectionHandler<T>>::Txn as TransactionHandler<T>>::Transaction>()
    }

    fn tick(&mut self) -> Poll<(), ::error::Void> {
        loop {
            let next_state;
            match self.state {
                State::Init { .. } => {
                    trace!("State::Init tick");
                    let (version, head) = match self.parse() {
                        Ok(Some(head)) => (head.version, Ok(head)),
                        Ok(None) => return Ok(Async::NotReady),
                        Err(e) => {
                            self.io.buf.consume_leading_lines();
                            if !self.io.buf.is_empty() {
                                trace!("parse error ({}) with bytes: {:?}", e, self.io.buf.bytes());
                                (HttpVersion::Http10, Err(e))
                            } else {
                                trace!("parse error with 0 input, err = {:?}", e);
                                self.state = State::Closed;
                                return Ok(Async::Ready(()));
                            }
                        }
                    };

                    match version {
                        HttpVersion::Http10 | HttpVersion::Http11 => {
                            let handler = match self.handler.transaction() {
                                Some(h) => h,
                                None => {
                                    trace!("could not create txn handler, key={:?}", self.key);
                                    self.state = State::Closed;
                                    return Ok(Async::Ready(()));
                                }
                            };
                            let res = head.and_then(|head| {
                                let decoder = <<H as ConnectionHandler<T>>::Txn as TransactionHandler<T>>::Transaction::decoder(&head);
                                decoder.map(move |decoder| (head, decoder))
                            });
                            next_state = State::Http1(h1::Txn::incoming(res, handler));
                        },
                        _ => {
                            warn!("unimplemented HTTP Version = {:?}", version);
                            self.state = State::Closed;
                            return Ok(Async::Ready(()));
                        }
                    }

                },
                State::Http1(ref mut http1) => {
                    trace!("State::Http1 tick");
                    match http1.tick(&mut self.io) {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(TxnResult::KeepAlive)) => {
                            trace!("Http1 Txn tick complete, keep-alive");
                            //TODO: check if keep-alive is enabled
                            next_state = State::Init {};
                        },
                        Ok(Async::Ready(TxnResult::Close)) => {
                            trace!("Http1 Txn tick complete, close");
                            next_state = State::Closed;
                        },
                        Err(void) => match void {}
                    }
                },
                //State::Http2
                State::Closed => {
                    trace!("State::Closed tick");
                    return Ok(Async::Ready(()));
                }
            }

            self.state = next_state;
        }
    }
    // TODO: leave this in the ConnectionHandler
    pub fn keep_alive(mut self, val: bool) -> Conn<K, T, H> {
        self.keep_alive_enabled = val;
        self
    }

    pub fn poll(&mut self) -> Poll<(), ::error::Void> {
        trace!("Conn::poll >> key={:?}", self.key);
        /*
        let was_init = match self.0.state {
            State::Init { .. } => true,
            _ => false
        };
        */

        let res = self.tick();

        trace!("Conn::poll << key={:?}, result={:?}", self.key, res);

        res

        //TODO: support http1 pipeline
        /*
        match tick {
            Tick::Final => Ok(Tick::Final),
            _ => {
                if self.can_read_more(was_init) {
                    self.ready()
                } else {
                    Ok(Tick::WouldBlock)
                }
            }
        }
        */
    }


    pub fn key(&self) -> &K {
        &self.key
    }


    /*
    pub fn is_idle(&self) -> bool {
        if let State::Init { interest: Next_::Wait, .. } = self.state {
            true
        } else {
            false
        }
    }
    */
}


impl<K: Key, T: Transport, H: ConnectionHandler<T>> fmt::Debug for Conn<K, T, H> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Conn")
            .field("keep_alive_enabled", &self.keep_alive_enabled)
            .field("state", &self.state)
            .field("io", &self.io)
            .finish()
    }
}

enum State<H: TransactionHandler<T>, T: Transport> {
    Init {
    },
    /// Http1 will only ever use a connection to send and receive a single
    /// message at a time. Once a H1 status has been determined, we will either
    /// be reading or writing an H1 message, and optionally multiple if
    /// keep-alive is true.
    Http1(h1::Txn<H, T>),
    /// Http2 allows multiplexing streams over a single connection. So even
    /// when we've identified a certain message, we must always parse frame
    /// head to determine if the incoming frame is part of a current message,
    /// or a new one. This also means we could have multiple messages at once.
    //Http2 {},
    Closed,
}


/*
impl<H: TransactionHandler<T>, T: Transport> State<H, T> {
    fn timeout(&self) -> Option<Duration> {
        match *self {
            State::Init { timeout, .. } => timeout,
            State::Http1(ref http1) => http1.timeout,
            State::Closed => None,
        }
    }
}
*/

impl<H: TransactionHandler<T>, T: Transport> fmt::Debug for State<H, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            State::Init { .. } => f.debug_struct("Init")
                .finish(),
            State::Http1(ref h1) => f.debug_tuple("Http1")
                .field(h1)
                .finish(),
            State::Closed => f.write_str("Closed")
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TxnResult {
    KeepAlive,
    Close
}

/*
impl<H: TransactionHandler<T>, T: Transport> State<H, T> {
    fn update(&mut self, next: Next) {
            let timeout = next.timeout;
            let state = mem::replace(self, State::Closed);
            match (state, next.interest) {
                (_, Next_::Remove) |
                (State::Closed, _) => return, // Keep State::Closed.
                (State::Init { .. }, e) => {
                    mem::replace(self,
                                 State::Init {
                                     interest: e,
                                     timeout: timeout,
                                 });
                }
                (State::Http1(mut http1), next_) => {
                    match next_ {
                        Next_::Remove => unreachable!(), // Covered in (_, Next_::Remove) case above.
                        Next_::End => {
                            let reading = match http1.reading {
                                Reading::Body(ref decoder) |
                                Reading::Wait(ref decoder) if decoder.is_eof() => {
                                    if http1.keep_alive {
                                        Reading::KeepAlive
                                    } else {
                                        Reading::Closed
                                    }
                                }
                                Reading::KeepAlive => http1.reading,
                                _ => Reading::Closed,
                            };
                            let mut writing = Writing::Closed;
                            let encoder = match http1.writing {
                                Writing::Wait(enc) |
                                Writing::Ready(enc) => Some(enc),
                                Writing::Chunk(mut chunk) => {
                                    if chunk.is_written() {
                                        Some(chunk.next.0)
                                    } else {
                                        chunk.next.1 = next;
                                        writing = Writing::Chunk(chunk);
                                        None
                                    }
                                }
                                _ => return, // Keep State::Closed.
                            };
                            if let Some(encoder) = encoder {
                                if encoder.is_eof() {
                                    if http1.keep_alive {
                                        writing = Writing::KeepAlive
                                    }
                                } else if let Some(buf) = encoder.finish() {
                                    writing = Writing::Chunk(Chunk {
                                        buf: buf.bytes,
                                        pos: buf.pos,
                                        next: (h1::Encoder::length(0), Next::end()),
                                    })
                                }
                            };

                            match (reading, writing) {
                                (Reading::KeepAlive, Writing::KeepAlive) => {
                                    let next = Next::read(); /*TODO: factory.keep_alive_interest();*/
                                    mem::replace(self,
                                                 State::Init {
                                                     interest: next.interest,
                                                     timeout: next.timeout,
                                                 });
                                    return;
                                }
                                (reading, Writing::Chunk(chunk)) => {
                                    http1.reading = reading;
                                    http1.writing = Writing::Chunk(chunk);
                                }
                                _ => return, // Keep State::Closed.
                            }
                        }
                        Next_::Read => {
                            http1.reading = match http1.reading {
                                Reading::Init => Reading::Parse,
                                Reading::Wait(decoder) => Reading::Body(decoder),
                                same => same,
                            };

                            http1.writing = match http1.writing {
                                Writing::Ready(encoder) => {
                                    if encoder.is_eof() {
                                        if http1.keep_alive {
                                            Writing::KeepAlive
                                        } else {
                                            Writing::Closed
                                        }
                                    } else if encoder.is_closed() {
                                        if let Some(buf) = encoder.finish() {
                                            Writing::Chunk(Chunk {
                                                buf: buf.bytes,
                                                pos: buf.pos,
                                                next: (h1::Encoder::length(0), Next::wait()),
                                            })
                                        } else {
                                            Writing::Closed
                                        }
                                    } else {
                                        Writing::Wait(encoder)
                                    }
                                }
                                Writing::Chunk(chunk) => {
                                    if chunk.is_written() {
                                        Writing::Wait(chunk.next.0)
                                    } else {
                                        Writing::Chunk(chunk)
                                    }
                                }
                                same => same,
                            };
                        }
                        Next_::Write => {
                            http1.writing = match http1.writing {
                                Writing::Wait(encoder) => Writing::Ready(encoder),
                                Writing::Init => Writing::Head,
                                Writing::Chunk(chunk) => {
                                    if chunk.is_written() {
                                        Writing::Ready(chunk.next.0)
                                    } else {
                                        Writing::Chunk(chunk)
                                    }
                                }
                                same => same,
                            };

                            http1.reading = match http1.reading {
                                Reading::Body(decoder) => {
                                    if decoder.is_eof() {
                                        if http1.keep_alive {
                                            Reading::KeepAlive
                                        } else {
                                            Reading::Closed
                                        }
                                    } else {
                                        Reading::Wait(decoder)
                                    }
                                }
                                same => same,
                            };
                        }
                        Next_::ReadWrite => {
                            http1.reading = match http1.reading {
                                Reading::Init => Reading::Parse,
                                Reading::Wait(decoder) => Reading::Body(decoder),
                                same => same,
                            };
                            http1.writing = match http1.writing {
                                Writing::Wait(encoder) => Writing::Ready(encoder),
                                Writing::Init => Writing::Head,
                                Writing::Chunk(chunk) => {
                                    if chunk.is_written() {
                                        Writing::Ready(chunk.next.0)
                                    } else {
                                        Writing::Chunk(chunk)
                                    }
                                }
                                same => same,
                            };
                        }
                        Next_::Wait => {
                            http1.reading = match http1.reading {
                                Reading::Body(decoder) => Reading::Wait(decoder),
                                same => same,
                            };

                            http1.writing = match http1.writing {
                                Writing::Ready(encoder) => Writing::Wait(encoder),
                                Writing::Chunk(chunk) => {
                                    if chunk.is_written() {
                                        Writing::Wait(chunk.next.0)
                                    } else {
                                        Writing::Chunk(chunk)
                                    }
                                }
                                same => same,
                            };
                        }
                    }
                    http1.timeout = timeout;
                    mem::replace(self, State::Http1(http1));
                }
            };
        }
}
*/

pub trait TransactionHandler<T: Transport> {
    type Transaction: Http1Transaction;

    fn ready(&mut self, txn: &mut http::Transaction<T, Self::Transaction>);
}

pub trait ConnectionHandler<T: Transport> {
    type Txn: TransactionHandler<T>;
    fn transaction(&mut self) -> Option<Self::Txn>;
    //fn keep_alive_interest(&self) -> Next;
}

pub trait ConnectionHandlerFactory<K: Key, T: Transport> {
    type Output: ConnectionHandler<T>;
    fn create(&mut self, seed: Seed<K>) -> Option<Self::Output>;
}

pub trait Key: Eq + Hash + Clone + fmt::Debug {}
impl<T: Eq + Hash + Clone + fmt::Debug> Key for T {}

pub struct Seed<'a, K: Key + 'a>(&'a K);

impl<'a, K: Key + 'a> Seed<'a, K> {
    pub fn key(&self) -> &K {
        self.0
    }
}


#[cfg(test)]
mod tests {
}
