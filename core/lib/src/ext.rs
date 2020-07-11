use std::io::{self, Cursor};
use std::pin::Pin;
use std::task::{Poll, Context};

use futures_core::{ready, future::BoxFuture, stream::Stream};
use tokio_io::{AsyncRead, AsyncReadExt as _};

use crate::http::hyper;
use hyper::{Chunk, Payload};

pub struct IntoChunkStream<R> {
    inner: R,
    buf_size: usize,
    buffer: Vec<u8>,
}

// TODO.async: Verify correctness of this implementation.
impl<R> Stream for IntoChunkStream<R>
    where R: AsyncRead + Unpin
{
    type Item = Result<Chunk, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>>{
        assert!(self.buffer.len() == self.buf_size);

        let Self { ref mut inner, ref mut buffer, buf_size } = *self;

        match Pin::new(inner).poll_read(cx, &mut buffer[..]) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Ready(Ok(n)) if n == 0 => Poll::Ready(None),
            Poll::Ready(Ok(n)) => {
                let mut next = std::mem::replace(buffer, vec![0; buf_size]);
                next.truncate(n);
                Poll::Ready(Some(Ok(Chunk::from(next))))
            }
        }
    }
}

pub trait AsyncReadExt: AsyncRead {
    fn into_chunk_stream(self, buf_size: usize) -> IntoChunkStream<Self> where Self: Sized {
        IntoChunkStream { inner: self, buf_size, buffer: vec![0; buf_size] }
    }

    // TODO.async: Verify correctness of this implementation.
    fn read_max<'a>(&'a mut self, mut buf: &'a mut [u8]) -> BoxFuture<'_, io::Result<usize>>
        where Self: Send + Unpin
    {
        Box::pin(async move {
            let start_len = buf.len();
            while !buf.is_empty() {
                match self.read(buf).await {
                    Ok(0) => break,
                    Ok(n) => { let tmp = buf; buf = &mut tmp[n..]; }
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }

            Ok(start_len - buf.len())
        })
    }
}

impl<T: AsyncRead> AsyncReadExt for T { }

pub struct AsyncReadBody {
    inner: hyper::Body,
    state: AsyncReadBodyState,
}

enum AsyncReadBodyState {
    Pending,
    Partial(Cursor<Chunk>),
    Done,
}

impl From<hyper::Body> for AsyncReadBody {
    fn from(body: hyper::Body) -> Self {
        Self { inner: body, state: AsyncReadBodyState::Pending }
    }
}

impl AsyncRead for AsyncReadBody {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        loop {
            match self.state {
                AsyncReadBodyState::Pending => {
                    match ready!(Pin::new(&mut self.inner).poll_data(cx)) {
                        Some(Ok(chunk)) => self.state = AsyncReadBodyState::Partial(Cursor::new(chunk)),
                        Some(Err(e)) => return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
                        None => self.state = AsyncReadBodyState::Done,
                    }
                },
                AsyncReadBodyState::Partial(ref mut cursor) => {
                    match ready!(Pin::new(cursor).poll_read(cx, buf)) {
                        Ok(n) if n == 0 => {
                            self.state = AsyncReadBodyState::Pending;
                        }
                        Ok(n) => return Poll::Ready(Ok(n)),
                        Err(e) => return Poll::Ready(Err(e)),
                    }
                }
                AsyncReadBodyState::Done => return Poll::Ready(Ok(0)),
            }
        }
    }
}
