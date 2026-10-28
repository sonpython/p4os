//! Minimal protobuf wire-format encoder/decoder (proto3: varints and
//! length-delimited fields only), enough for ESP-Hosted RPC messages.

use alloc::vec::Vec;

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    fn varint(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(b);
                return;
            }
            self.buf.push(b | 0x80);
        }
    }

    fn key(&mut self, field: u32, wire: u8) {
        self.varint(((field as u64) << 3) | wire as u64);
    }

    /// int32/uint32/bool/enum field (negative values as 10-byte varints).
    pub fn int(&mut self, field: u32, v: i64) -> &mut Self {
        self.key(field, 0);
        self.varint(v as u64);
        self
    }

    pub fn bytes(&mut self, field: u32, data: &[u8]) -> &mut Self {
        self.key(field, 2);
        self.varint(data.len() as u64);
        self.buf.extend_from_slice(data);
        self
    }

    pub fn message(&mut self, field: u32, inner: &Writer) -> &mut Self {
        self.bytes(field, &inner.buf)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Value<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

/// Iterate (field number, value) pairs; stops at the first malformed byte.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.data.get(self.pos)?;
            self.pos += 1;
            v |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    /// Value of the first occurrence of `field`.
    pub fn find(data: &'a [u8], field: u32) -> Option<Value<'a>> {
        Reader::new(data).find_map(|(f, v)| (f == field).then_some(v))
    }
}

impl<'a> Iterator for Reader<'a> {
    type Item = (u32, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let key = self.varint()?;
        let field = (key >> 3) as u32;
        match key & 7 {
            0 => Some((field, Value::Int(self.varint()?))),
            2 => {
                let len = self.varint()? as usize;
                let v = self.data.get(self.pos..self.pos + len)?;
                self.pos += len;
                Some((field, Value::Bytes(v)))
            }
            1 => {
                self.pos += 8;
                Some((field, Value::Int(0)))
            }
            5 => {
                self.pos += 4;
                Some((field, Value::Int(0)))
            }
            _ => None,
        }
    }
}

pub fn int(v: Option<Value>) -> i64 {
    match v {
        Some(Value::Int(i)) => i as i64,
        _ => 0,
    }
}
