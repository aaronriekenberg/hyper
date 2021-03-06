use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;
use std::io;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::{Async, Poll};
use iovec::IoVec;
use tokio_io::{AsyncRead, AsyncWrite};

use super::{Http1Transaction, ParseContext, ParsedMessage};

/// The initial buffer size allocated before trying to read from IO.
pub(crate) const INIT_BUFFER_SIZE: usize = 8192;

/// The minimum value that can be set to max buffer size.
pub const MINIMUM_MAX_BUFFER_SIZE: usize = INIT_BUFFER_SIZE;

/// The default maximum read buffer size. If the buffer gets this big and
/// a message is still not complete, a `TooLarge` error is triggered.
// Note: if this changes, update server::conn::Http::max_buf_size docs.
pub(crate) const DEFAULT_MAX_BUFFER_SIZE: usize = 8192 + 4096 * 100;

/// The maximum number of distinct `Buf`s to hold in a list before requiring
/// a flush. Only affects when the buffer strategy is to queue buffers.
///
/// Note that a flush can happen before reaching the maximum. This simply
/// forces a flush if the queue gets this big.
const MAX_BUF_LIST_BUFFERS: usize = 16;

pub struct Buffered<T, B> {
    flush_pipeline: bool,
    io: T,
    max_buf_size: usize,
    read_blocked: bool,
    read_buf: BytesMut,
    write_buf: WriteBuf<B>,
}

impl<T, B> fmt::Debug for Buffered<T, B>
where
    B: Buf,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Buffered")
            .field("read_buf", &self.read_buf)
            .field("write_buf", &self.write_buf)
            .finish()
    }
}

impl<T, B> Buffered<T, B>
where
    T: AsyncRead + AsyncWrite,
    B: Buf,
{
    pub fn new(io: T) -> Buffered<T, B> {
        Buffered {
            flush_pipeline: false,
            io: io,
            max_buf_size: DEFAULT_MAX_BUFFER_SIZE,
            read_buf: BytesMut::with_capacity(0),
            write_buf: WriteBuf::new(),
            read_blocked: false,
        }
    }

    pub fn set_flush_pipeline(&mut self, enabled: bool) {
        self.flush_pipeline = enabled;
        self.write_buf.set_strategy(if enabled {
            Strategy::Flatten
        } else {
            Strategy::Auto
        });
    }

    pub fn set_max_buf_size(&mut self, max: usize) {
        assert!(
            max >= MINIMUM_MAX_BUFFER_SIZE,
            "The max_buf_size cannot be smaller than the initial buffer size."
        );
        self.max_buf_size = max;
        self.write_buf.max_buf_size = max;
    }

    pub fn set_write_strategy_flatten(&mut self) {
        // this should always be called only at construction time,
        // so this assert is here to catch myself
        debug_assert!(self.write_buf.queue.bufs.is_empty());
        self.write_buf.set_strategy(Strategy::Flatten);
    }

    pub fn read_buf(&self) -> &[u8] {
        self.read_buf.as_ref()
    }

    pub fn headers_buf(&mut self) -> &mut Vec<u8> {
        let buf = self.write_buf.headers_mut();
        &mut buf.bytes
    }

    pub(super) fn write_buf(&mut self) -> &mut WriteBuf<B> {
        &mut self.write_buf
    }

    pub fn buffer<BB: Buf + Into<B>>(&mut self, buf: BB) {
        self.write_buf.buffer(buf)
    }

    pub fn can_buffer(&self) -> bool {
        self.flush_pipeline || self.write_buf.can_buffer()
    }

    pub fn consume_leading_lines(&mut self) {
        if !self.read_buf.is_empty() {
            let mut i = 0;
            while i < self.read_buf.len() {
                match self.read_buf[i] {
                    b'\r' | b'\n' => i += 1,
                    _ => break,
                }
            }
            self.read_buf.split_to(i);
        }
    }

    pub(super) fn parse<S>(&mut self, ctx: ParseContext)
        -> Poll<ParsedMessage<S::Incoming>, ::Error>
    where
        S: Http1Transaction,
    {
        loop {
            match try!(S::parse(&mut self.read_buf, ParseContext { cached_headers: ctx.cached_headers, req_method: ctx.req_method, })) {
                Some(msg) => {
                    debug!("parsed {} headers", msg.head.headers.len());
                    return Ok(Async::Ready(msg))
                },
                None => {
                    if self.read_buf.capacity() >= self.max_buf_size {
                        debug!("max_buf_size ({}) reached, closing", self.max_buf_size);
                        return Err(::Error::new_too_large());
                    }
                },
            }
            match try_ready!(self.read_from_io().map_err(::Error::new_io)) {
                0 => {
                    trace!("parse eof");
                    return Err(::Error::new_incomplete());
                }
                _ => {},
            }
        }
    }

    pub fn read_from_io(&mut self) -> Poll<usize, io::Error> {
        use bytes::BufMut;
        self.read_blocked = false;
        if self.read_buf.remaining_mut() < INIT_BUFFER_SIZE {
            self.read_buf.reserve(INIT_BUFFER_SIZE);
        }
        self.io.read_buf(&mut self.read_buf).map(|ok| {
            match ok {
                Async::Ready(n) => {
                    debug!("read {} bytes", n);
                    Async::Ready(n)
                },
                Async::NotReady => {
                    self.read_blocked = true;
                    Async::NotReady
                }
            }
        })
    }

    pub fn into_inner(self) -> (T, Bytes) {
        (self.io, self.read_buf.freeze())
    }

    pub fn io_mut(&mut self) -> &mut T {
        &mut self.io
    }

    pub fn is_read_blocked(&self) -> bool {
        self.read_blocked
    }

    pub fn flush(&mut self) -> Poll<(), io::Error> {
        if self.flush_pipeline && !self.read_buf.is_empty() {
            //Ok(())
        } else if self.write_buf.remaining() == 0 {
            try_nb!(self.io.flush());
        } else {
            match self.write_buf.strategy {
                Strategy::Flatten => return self.flush_flattened(),
                _ => (),
            }
            loop {
                let n = try_ready!(self.io.write_buf(&mut self.write_buf.auto()));
                debug!("flushed {} bytes", n);
                if self.write_buf.remaining() == 0 {
                    break;
                } else if n == 0 {
                    trace!("write returned zero, but {} bytes remaining", self.write_buf.remaining());
                    return Err(io::ErrorKind::WriteZero.into())
                }
            }
            try_nb!(self.io.flush())
        }
        Ok(Async::Ready(()))
    }

    /// Specialized version of `flush` when strategy is Flatten.
    ///
    /// Since all buffered bytes are flattened into the single headers buffer,
    /// that skips some bookkeeping around using multiple buffers.
    fn flush_flattened(&mut self) -> Poll<(), io::Error> {
        loop {
            let n = try_nb!(self.io.write(self.write_buf.headers.bytes()));
            debug!("flushed {} bytes", n);
            self.write_buf.headers.advance(n);
            if self.write_buf.headers.remaining() == 0 {
                self.write_buf.headers.reset();
                break;
            } else if n == 0 {
                trace!("write returned zero, but {} bytes remaining", self.write_buf.remaining());
                return Err(io::ErrorKind::WriteZero.into())
            }
        }
        try_nb!(self.io.flush());
        Ok(Async::Ready(()))
    }
}

pub trait MemRead {
    fn read_mem(&mut self, len: usize) -> Poll<Bytes, io::Error>;
}

impl<T, B> MemRead for Buffered<T, B> 
where
    T: AsyncRead + AsyncWrite,
    B: Buf,
{
    fn read_mem(&mut self, len: usize) -> Poll<Bytes, io::Error> {
        if !self.read_buf.is_empty() {
            let n = ::std::cmp::min(len, self.read_buf.len());
            Ok(Async::Ready(self.read_buf.split_to(n).freeze()))
        } else {
            let n = try_ready!(self.read_from_io());
            Ok(Async::Ready(self.read_buf.split_to(::std::cmp::min(len, n)).freeze()))
        }
    }
}

#[derive(Clone)]
pub struct Cursor<T> {
    bytes: T,
    pos: usize,
}

impl<T: AsRef<[u8]>> Cursor<T> {
    #[inline]
    pub(crate) fn new(bytes: T) -> Cursor<T> {
        Cursor {
            bytes: bytes,
            pos: 0,
        }
    }
}

impl Cursor<Vec<u8>> {
    fn reset(&mut self) {
        self.pos = 0;
        unsafe {
            self.bytes.set_len(0);
        }
    }
}

impl<T: AsRef<[u8]>> fmt::Debug for Cursor<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Cursor")
            .field("pos", &self.pos)
            .field("len", &self.bytes.as_ref().len())
            .finish()
    }
}

impl<T: AsRef<[u8]>> Buf for Cursor<T> {
    #[inline]
    fn remaining(&self) -> usize {
        self.bytes.as_ref().len() - self.pos
    }

    #[inline]
    fn bytes(&self) -> &[u8] {
        &self.bytes.as_ref()[self.pos..]
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        debug_assert!(self.pos + cnt <= self.bytes.as_ref().len());
        self.pos += cnt;
    }
}

// an internal buffer to collect writes before flushes
pub(super) struct WriteBuf<B> {
    /// Re-usable buffer that holds message headers
    headers: Cursor<Vec<u8>>,
    max_buf_size: usize,
    /// Deque of user buffers if strategy is Queue
    queue: BufDeque<B>,
    strategy: Strategy,
}

impl<B> WriteBuf<B> {
    fn new() -> WriteBuf<B> {
        WriteBuf {
            headers: Cursor::new(Vec::with_capacity(INIT_BUFFER_SIZE)),
            max_buf_size: DEFAULT_MAX_BUFFER_SIZE,
            queue: BufDeque::new(),
            strategy: Strategy::Auto,
        }
    }
}


impl<B> WriteBuf<B>
where
    B: Buf,
{
    fn set_strategy(&mut self, strategy: Strategy) {
        self.strategy = strategy;
    }

    #[inline]
    fn auto(&mut self) -> WriteBufAuto<B> {
        WriteBufAuto::new(self)
    }

    pub(super) fn buffer<BB: Buf + Into<B>>(&mut self, mut buf: BB) {
        debug_assert!(buf.has_remaining());
        match self.strategy {
            Strategy::Flatten => {
                let head = self.headers_mut();
                //perf: This is a little faster than <Vec as BufMut>>::put,
                //but accomplishes the same result.
                loop {
                    let adv = {
                        let slice = buf.bytes();
                        if slice.is_empty() {
                            return;
                        }
                        head.bytes.extend_from_slice(slice);
                        slice.len()
                    };
                    buf.advance(adv);
                }
            },
            Strategy::Auto | Strategy::Queue => {
                self.queue.bufs.push_back(buf.into());
            },
        }
    }

    fn can_buffer(&self) -> bool {
        match self.strategy {
            Strategy::Flatten => {
                self.remaining() < self.max_buf_size
            },
            Strategy::Auto | Strategy::Queue => {
                self.queue.bufs.len() < MAX_BUF_LIST_BUFFERS
                    && self.remaining() < self.max_buf_size
            },
        }
    }

    fn headers_mut(&mut self) -> &mut Cursor<Vec<u8>> {
        debug_assert!(!self.queue.has_remaining());
        &mut self.headers
    }
}

impl<B: Buf> fmt::Debug for WriteBuf<B> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("WriteBuf")
            .field("remaining", &self.remaining())
            .field("strategy", &self.strategy)
            .finish()
    }
}

impl<B: Buf> Buf for WriteBuf<B> {
    #[inline]
    fn remaining(&self) -> usize {
        self.headers.remaining() + self.queue.remaining()
    }

    #[inline]
    fn bytes(&self) -> &[u8] {
        let headers = self.headers.bytes();
        if !headers.is_empty() {
            headers
        } else {
            self.queue.bytes()
        }
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        let hrem = self.headers.remaining();
        if hrem == cnt {
            self.headers.reset();
        } else if hrem > cnt {
            self.headers.advance(cnt);
        } else {
            let qcnt = cnt - hrem;
            self.headers.reset();
            self.queue.advance(qcnt);
        }
    }

    #[inline]
    fn bytes_vec<'t>(&'t self, dst: &mut [&'t IoVec]) -> usize {
        let n = self.headers.bytes_vec(dst);
        self.queue.bytes_vec(&mut dst[n..]) + n
    }
}

/// Detects when wrapped `WriteBuf` is used for vectored IO, and
/// adjusts the `WriteBuf` strategy if not.
struct WriteBufAuto<'a, B: Buf + 'a> {
    bytes_called: Cell<bool>,
    bytes_vec_called: Cell<bool>,
    inner: &'a mut WriteBuf<B>,
}

impl<'a, B: Buf> WriteBufAuto<'a, B> {
    fn new(inner: &'a mut WriteBuf<B>) -> WriteBufAuto<'a, B> {
        WriteBufAuto {
            bytes_called: Cell::new(false),
            bytes_vec_called: Cell::new(false),
            inner: inner,
        }
    }
}

impl<'a, B: Buf> Buf for WriteBufAuto<'a, B> {
    #[inline]
    fn remaining(&self) -> usize {
        self.inner.remaining()
    }

    #[inline]
    fn bytes(&self) -> &[u8] {
        self.bytes_called.set(true);
        self.inner.bytes()
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        self.inner.advance(cnt)
    }

    #[inline]
    fn bytes_vec<'t>(&'t self, dst: &mut [&'t IoVec]) -> usize {
        self.bytes_vec_called.set(true);
        self.inner.bytes_vec(dst)
    }
}

impl<'a, B: Buf + 'a> Drop for WriteBufAuto<'a, B> {
    fn drop(&mut self) {
        if let Strategy::Auto = self.inner.strategy {
            if self.bytes_vec_called.get() {
                self.inner.strategy = Strategy::Queue;
            } else if self.bytes_called.get() {
                trace!("detected no usage of vectored write, flattening");
                self.inner.strategy = Strategy::Flatten;
                self.inner.headers.bytes.put(&mut self.inner.queue);
            }
        }
    }
}


#[derive(Debug)]
enum Strategy {
    Auto,
    Flatten,
    Queue,
}

struct BufDeque<T> {
    bufs: VecDeque<T>,
}


impl<T> BufDeque<T> {
    fn new() -> BufDeque<T> {
        BufDeque {
            bufs: VecDeque::new(),
        }
    }
}

impl<T: Buf> Buf for BufDeque<T> {
    #[inline]
    fn remaining(&self) -> usize {
        self.bufs.iter()
            .map(|buf| buf.remaining())
            .sum()
    }

    #[inline]
    fn bytes(&self) -> &[u8] {
        for buf in &self.bufs {
            return buf.bytes();
        }
        &[]
    }

    #[inline]
    fn advance(&mut self, mut cnt: usize) {
        while cnt > 0 {
            {
                let front = &mut self.bufs[0];
                let rem = front.remaining();
                if rem > cnt {
                    front.advance(cnt);
                    return;
                } else {
                    front.advance(rem);
                    cnt -= rem;
                }
            }
            self.bufs.pop_front();
        }
    }

    #[inline]
    fn bytes_vec<'t>(&'t self, dst: &mut [&'t IoVec]) -> usize {
        if dst.is_empty() {
            return 0;
        }
        let mut vecs = 0;
        for buf in &self.bufs {
            vecs += buf.bytes_vec(&mut dst[vecs..]);
            if vecs == dst.len() {
                break;
            }
        }
        vecs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use mock::AsyncIo;

    #[cfg(feature = "nightly")]
    use test::Bencher;

    #[cfg(test)]
    impl<T: Read> MemRead for ::mock::AsyncIo<T> {
        fn read_mem(&mut self, len: usize) -> Poll<Bytes, io::Error> {
            let mut v = vec![0; len];
            let n = try_nb!(self.read(v.as_mut_slice()));
            Ok(Async::Ready(BytesMut::from(&v[..n]).freeze()))
        }
    }

    #[test]
    fn iobuf_write_empty_slice() {
        let mut mock = AsyncIo::new_buf(vec![], 256);
        mock.error(io::Error::new(io::ErrorKind::Other, "logic error"));

        let mut io_buf = Buffered::<_, Cursor<Vec<u8>>>::new(mock);

        // underlying io will return the logic error upon write,
        // so we are testing that the io_buf does not trigger a write
        // when there is nothing to flush
        io_buf.flush().expect("should short-circuit flush");
    }

    #[test]
    fn parse_reads_until_blocked() {
        // missing last line ending
        let raw = "HTTP/1.1 200 OK\r\n";

        let mock = AsyncIo::new_buf(raw, raw.len());
        let mut buffered = Buffered::<_, Cursor<Vec<u8>>>::new(mock);
        let ctx = ParseContext {
            cached_headers: &mut None,
            req_method: &mut None,
        };
        assert!(buffered.parse::<::proto::ClientTransaction>(ctx).unwrap().is_not_ready());
        assert!(buffered.io.blocked());
    }

    #[test]
    #[should_panic]
    fn write_buf_requires_non_empty_bufs() {
        let mock = AsyncIo::new_buf(vec![], 1024);
        let mut buffered = Buffered::<_, Cursor<Vec<u8>>>::new(mock);

        buffered.buffer(Cursor::new(Vec::new()));
    }

    #[test]
    fn write_buf_queue() {
        extern crate pretty_env_logger;
        let _ = pretty_env_logger::try_init();

        let mock = AsyncIo::new_buf(vec![], 1024);
        let mut buffered = Buffered::<_, Cursor<Vec<u8>>>::new(mock);


        buffered.headers_buf().extend(b"hello ");
        buffered.buffer(Cursor::new(b"world, ".to_vec()));
        buffered.buffer(Cursor::new(b"it's ".to_vec()));
        buffered.buffer(Cursor::new(b"hyper!".to_vec()));
        assert_eq!(buffered.write_buf.queue.bufs.len(), 3);
        buffered.flush().unwrap();

        assert_eq!(buffered.io, b"hello world, it's hyper!");
        assert_eq!(buffered.io.num_writes(), 1);
        assert_eq!(buffered.write_buf.queue.bufs.len(), 0);
    }

    #[test]
    fn write_buf_flatten() {
        extern crate pretty_env_logger;
        let _ = pretty_env_logger::try_init();

        let mock = AsyncIo::new_buf(vec![], 1024);
        let mut buffered = Buffered::<_, Cursor<Vec<u8>>>::new(mock);
        buffered.write_buf.set_strategy(Strategy::Flatten);

        buffered.headers_buf().extend(b"hello ");
        buffered.buffer(Cursor::new(b"world, ".to_vec()));
        buffered.buffer(Cursor::new(b"it's ".to_vec()));
        buffered.buffer(Cursor::new(b"hyper!".to_vec()));
        assert_eq!(buffered.write_buf.queue.bufs.len(), 0);

        buffered.flush().unwrap();

        assert_eq!(buffered.io, b"hello world, it's hyper!");
        assert_eq!(buffered.io.num_writes(), 1);
    }

    #[test]
    fn write_buf_auto_flatten() {
        extern crate pretty_env_logger;
        let _ = pretty_env_logger::try_init();

        let mut mock = AsyncIo::new_buf(vec![], 1024);
        mock.max_read_vecs(0); // disable vectored IO
        let mut buffered = Buffered::<_, Cursor<Vec<u8>>>::new(mock);

        // we have 4 buffers, but hope to detect that vectored IO isn't
        // being used, and switch to flattening automatically,
        // resulting in only 2 writes
        buffered.headers_buf().extend(b"hello ");
        buffered.buffer(Cursor::new(b"world, ".to_vec()));
        buffered.buffer(Cursor::new(b"it's ".to_vec()));
        buffered.buffer(Cursor::new(b"hyper!".to_vec()));
        assert_eq!(buffered.write_buf.queue.bufs.len(), 3);
        buffered.flush().unwrap();

        assert_eq!(buffered.io, b"hello world, it's hyper!");
        assert_eq!(buffered.io.num_writes(), 2);
        assert_eq!(buffered.write_buf.queue.bufs.len(), 0);
    }

    #[test]
    fn write_buf_queue_disable_auto() {
        extern crate pretty_env_logger;
        let _ = pretty_env_logger::try_init();

        let mut mock = AsyncIo::new_buf(vec![], 1024);
        mock.max_read_vecs(0); // disable vectored IO
        let mut buffered = Buffered::<_, Cursor<Vec<u8>>>::new(mock);
        buffered.write_buf.set_strategy(Strategy::Queue);

        // we have 4 buffers, and vec IO disabled, but explicitly said
        // don't try to auto detect (via setting strategy above)

        buffered.headers_buf().extend(b"hello ");
        buffered.buffer(Cursor::new(b"world, ".to_vec()));
        buffered.buffer(Cursor::new(b"it's ".to_vec()));
        buffered.buffer(Cursor::new(b"hyper!".to_vec()));
        assert_eq!(buffered.write_buf.queue.bufs.len(), 3);
        buffered.flush().unwrap();

        assert_eq!(buffered.io, b"hello world, it's hyper!");
        assert_eq!(buffered.io.num_writes(), 4);
        assert_eq!(buffered.write_buf.queue.bufs.len(), 0);
    }

    #[cfg(feature = "nightly")]
    #[bench]
    fn bench_write_buf_flatten_buffer_chunk(b: &mut Bencher) {
        let s = "Hello, World!";
        b.bytes = s.len() as u64;

        let mut write_buf = WriteBuf::<::Chunk>::new();
        write_buf.set_strategy(Strategy::Flatten);
        b.iter(|| {
            let chunk = ::Chunk::from(s);
            write_buf.buffer(chunk);
            ::test::black_box(&write_buf);
            unsafe {
                write_buf.headers.bytes.set_len(0);
            }
        })
    }
}
