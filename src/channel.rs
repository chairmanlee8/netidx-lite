use crate::utils::BytesWriter;
use bytes::{BytesMut, Bytes, Buf, BufMut};
use async_std::{
    prelude::*,
    net::TcpStream,
};
use std::{
    mem,
    result::Result,
    io::{Error, ErrorKind},
};
use serde::{de::DeserializeOwned, Serialize};
use byteorder::{BigEndian, ByteOrder};

const BUF: usize = 4096;

/// RawChannel sends and receives u32 length prefixed messages, which
/// are otherwise just raw bytes.
pub(crate) struct Channel {
    socket: TcpStream,
    outgoing: BytesMut,
    incoming: BytesMut,
}

impl Channel {
    pub(crate) fn new(socket: TcpStream) -> Channel {
        Channel {
            socket,
            outgoing: BytesMut::with_capacity(BUF),
            incoming: BytesMut::with_capacity(BUF),
        }
    }

    /// Queue an outgoing message. This ONLY queues the message, use
    /// flush to initiate sending. It will fail if the message is
    /// larger then `u32::max_value()`.
    pub(crate) fn queue_send_raw(&mut self, msg: Bytes) -> Result<(), Error> {
        if msg.len() > u32::max_value() as usize {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("message too large {} > {}", msg.len(), u32::max_value())
            ));
        }
        if self.outgoing.remaining_mut() < mem::size_of::<u32>() {
            self.outgoing.reserve(self.outgoing.capacity());
        }
        self.outgoing.put_u32(msg.len() as u32);
        Ok(self.outgoing.extend_from_slice(&*msg))
    }

    /// Same as queue_send_raw, but encodes the message using msgpack
    pub(crate) fn queue_send<T: Serialize>(&mut self, msg: &T) -> Result<(), Error> {
        if self.outgoing.remaining_mut() < mem::size_of::<u32>() {
            self.outgoing.reserve(self.outgoing.capacity());
        }
        let mut msgbuf = self.outgoing.split_off(self.outgoing.len());
        msgbuf.put_u32(0);
        let mut header = msgbuf.split_to(msgbuf.len());
        header.clear();
        let r =
            rmp_serde::encode::write_named(&mut BytesWriter(&mut msgbuf), msg)
            .map_err(|e| Error::new(ErrorKind::InvalidData, e));
        match r {
            Ok(()) => {
                header.put_u32(msgbuf.len() as u32);
                header.unsplit(msgbuf);
                Ok(self.outgoing.unsplit(header))
            }
            Err(e) => {
                msgbuf.clear();
                header.unsplit(msgbuf);
                self.outgoing.unsplit(header);
                Err(e)
            }
        }
    }
    
    /// Initiate sending all outgoing messages and wait for the
    /// process to finish.
    pub(crate) async fn flush(&mut self) -> Result<(), Error> {
        let r = self.socket.write_all(&*self.outgoing).await;
        self.outgoing.clear();
        r
    }

    /// Queue one typed message and then flush.
    pub(crate) async fn send_one<T: Serialize>(&mut self, msg: &T) -> Result<(), Error> {
        self.queue_send(msg)?;
        self.flush().await
    }
    
    async fn fill_buffer(&mut self) -> Result<(), Error> {
        // it would be nice if we could read directly into the buf,
        // but we can't do that without unsafe code.
        let mut buf = [0; BUF];
        let n = self.socket.read(&mut buf).await?;
        if n <= 0 {
            Err(Error::new(ErrorKind::UnexpectedEof, "end of file"))
        } else {
            Ok(self.incoming.extend_from_slice(&buf[0..n]))
        }
    }

    fn decode_from_buffer(&mut self) -> Option<Bytes> {
        if self.incoming.remaining() < mem::size_of::<u32>() {
            None
        } else {
            let len = BigEndian::read_u32(&*self.incoming) as usize;
            if self.incoming.remaining() - mem::size_of::<u32>() < len {
                None
            } else {
                self.incoming.advance(mem::size_of::<u32>());
                Some(self.incoming.split_to(len).freeze())
            }
        }
    }

    /// Receive one message, potentially waiting for one to arrive if
    /// none are presently in the buffer.
    pub(crate) async fn receive_raw(&mut self) -> Result<Bytes, Error> {
        loop {
            match self.decode_from_buffer() {
                Some(msg) => break Ok(msg),
                None => { self.fill_buffer().await?; },
            }
        }
    }
    
    pub(crate) async fn receive<T: DeserializeOwned>(&mut self) -> Result<T, Error> {
        rmp_serde::decode::from_read(&*self.receive_raw().await?)
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))
    }

    /// Receive one or more messages.
    pub(crate) async fn receive_batch_raw(
        &mut self, batch: &mut Vec<Bytes>,
    ) -> Result<(), Error> {
        batch.push(self.receive_raw().await?);
        while let Some(b) = self.decode_from_buffer() {
            batch.push(b);
        }
        Ok(())
    }

    /// Receive and decode one or more messages. If any messages fails
    /// to decode, processing will stop and error will be returned,
    /// however some messages may have already been put in the batch.
    pub(crate) async fn receive_batch<T: DeserializeOwned>(
        &mut self, batch: &mut Vec<T>,
    ) -> Result<(), Error> {
        batch.push(self.receive().await?);
        while let Some(b) = self.decode_from_buffer() {
            batch.push(rmp_serde::decode::from_read(&*b).map_err(|e| {
                Error::new(ErrorKind::InvalidData, e)
            })?);
        }
        Ok(())
    }
}
