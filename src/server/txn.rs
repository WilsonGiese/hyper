use std::marker::PhantomData;
use std::io;


use http;
use net::Transport;

use super::{HandlerFactory, Handler, request, response, Request, Response};

/// A `TransactionHandler` for a Server.
///
/// This should be really thin glue between `http::TransactionHandler` and
/// `server::Handler`, but largely just providing the proper types one
/// would expect in a Server Handler.
pub struct Handle<H: HandlerFactory<T>, T: Transport> {
    handler: Either<H, H::Output>,
    _marker: PhantomData<T>
}

enum Either<A, B> {
    A(A),
    B(B)
}

impl<H: HandlerFactory<T>, T: Transport> Handle<H, T> {
    pub fn new(factory: H) -> Handle<H, T> {
        Handle {
            handler: Either::A(factory),
            _marker: PhantomData,
        }
    }
}

impl<H: HandlerFactory<T>, T: Transport> http::TransactionHandler<T> for Handle<H, T> {
    type Transaction = http::ServerTransaction;

    #[inline]
    fn ready(&mut self, txn: &mut http::Transaction<T, Self::Transaction>) {
        let mut handler = match self.handler {
            Either::A(ref mut factory) => {
                let incoming = txn.incoming().map(request::new);
                match factory.create(incoming) {
                    Ok(handler) => handler,
                    Err(e) => {
                        error!("HandlerFactory.create returned err = {}", e);
                        txn.abort();
                        return;
                    }
                }

            },
            Either::B(ref mut handler) => {
                let mut outer = Transaction { inner: txn };
                handler.ready(&mut outer);
                return;
            }
        };

        let mut outer = Transaction { inner: txn };
        handler.ready(&mut outer);
        self.handler = Either::B(handler);
    }
}

pub struct Transaction<'a: 'b, 'b, T: Transport + 'a> {
    inner: &'b mut http::Transaction<'a, T, http::ServerTransaction>,
}

impl<'a: 'b, 'b, T: Transport + 'a> Transaction<'a, 'b, T> {
    /*
    #[inline]
    pub fn request(&mut self) -> ::Result<Request> {
        self.inner.incoming().map(request::new)
    }
    */

    #[inline]
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }

    #[inline]
    pub fn try_read(&mut self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        self.inner.try_read(buf)
    }

    #[inline]
    pub fn response(&mut self) -> Response {
        response::new(self.inner.outgoing())
    }

    #[inline]
    pub fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.inner.write(data)
    }

    #[inline]
    pub fn try_write(&mut self, data: &[u8]) -> io::Result<Option<usize>> {
        self.inner.try_write(data)
    }

    #[inline]
    pub fn end(&mut self) {
        self.inner.end();
    }
}