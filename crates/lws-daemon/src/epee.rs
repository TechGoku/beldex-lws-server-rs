//! epee "portable storage", the binary encoding beldexd uses for `*.bin` RPCs.
//!
//! Layout: two 32-bit signatures and a version byte, then the root section. A
//! section is a varint entry count followed by entries of
//! `name_len:u8 name type:u8 value`; arrays set bit 0x80 on the type and carry
//! a varint element count. Strings are length-prefixed byte blobs and are
//! returned as zero-copy slices of the input buffer.

use bytes::Bytes;

const SIGNATURE_A: u32 = 0x0101_1101;
const SIGNATURE_B: u32 = 0x0102_0101;
const VERSION: u8 = 1;
const MAX_DEPTH: usize = 32;

const T_I64: u8 = 1;
const T_I32: u8 = 2;
const T_I16: u8 = 3;
const T_I8: u8 = 4;
const T_U64: u8 = 5;
const T_U32: u8 = 6;
const T_U16: u8 = 7;
const T_U8: u8 = 8;
const T_F64: u8 = 9;
const T_STRING: u8 = 10;
const T_BOOL: u8 = 11;
const T_OBJECT: u8 = 12;
const T_ARRAY: u8 = 13;
const FLAG_ARRAY: u8 = 0x80;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("bad portable-storage signature")]
    Signature,
    #[error("truncated portable-storage data")]
    Truncated,
    #[error("unknown portable-storage type {0}")]
    Type(u8),
    #[error("portable-storage nesting too deep")]
    Depth,
    #[error("field '{0}' missing or of the wrong type")]
    Field(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Uint(u64),
    Float(f64),
    Bytes(Bytes),
    Bool(bool),
    Object(Section),
    Array(Vec<Value>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Section(pub Vec<(String, Value)>);

impl Section {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn u64(&self, name: &'static str) -> Result<u64, Error> {
        self.get(name).and_then(Value::as_u64).ok_or(Error::Field(name))
    }

    pub fn bytes(&self, name: &'static str) -> Result<&Bytes, Error> {
        match self.get(name) {
            Some(Value::Bytes(b)) => Ok(b),
            _ => Err(Error::Field(name)),
        }
    }

    /// An array field. epee omits empty containers, so a missing field is empty.
    pub fn array(&self, name: &'static str) -> Result<&[Value], Error> {
        match self.get(name) {
            None => Ok(&[]),
            Some(Value::Array(items)) => Ok(items),
            Some(_) => Err(Error::Field(name)),
        }
    }

    pub fn insert(&mut self, name: &str, value: Value) {
        self.0.push((name.to_owned(), value));
    }
}

impl Value {
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Value::Uint(v) => Some(v),
            Value::Int(v) => u64::try_from(v).ok(),
            _ => None,
        }
    }
    pub fn as_object(&self) -> Option<&Section> {
        match self {
            Value::Object(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&Bytes> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

struct Reader {
    buf: Bytes,
    pos: usize,
}

impl Reader {
    fn take(&mut self, n: usize) -> Result<Bytes, Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        if end > self.buf.len() {
            return Err(Error::Truncated);
        }
        let out = self.buf.slice(self.pos..end);
        self.pos = end;
        Ok(out)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let b = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(&b);
        Ok(out)
    }

    fn varint(&mut self) -> Result<u64, Error> {
        let first = *self.buf.get(self.pos).ok_or(Error::Truncated)?;
        let raw = match first & 0x03 {
            0 => u64::from(self.fixed::<1>()?[0]),
            1 => u64::from(u16::from_le_bytes(self.fixed()?)),
            2 => u64::from(u32::from_le_bytes(self.fixed()?)),
            _ => u64::from_le_bytes(self.fixed()?),
        };
        Ok(raw >> 2)
    }

    /// A count that can never exceed the bytes left (each element takes at
    /// least one), so a hostile length cannot trigger a huge allocation.
    fn count(&mut self) -> Result<usize, Error> {
        let n = self.varint()?;
        let left = (self.buf.len() - self.pos) as u64;
        if n > left {
            return Err(Error::Truncated);
        }
        Ok(n as usize)
    }

    fn section(&mut self, depth: usize) -> Result<Section, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::Depth);
        }
        let count = self.count()?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let len = self.fixed::<1>()?[0] as usize;
            let name = String::from_utf8_lossy(&self.take(len)?).into_owned();
            let ty = self.fixed::<1>()?[0];
            let value = if ty & FLAG_ARRAY != 0 {
                let elem = ty & !FLAG_ARRAY;
                let n = self.count()?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.value(elem, depth + 1)?);
                }
                Value::Array(items)
            } else {
                self.value(ty, depth + 1)?
            };
            entries.push((name, value));
        }
        Ok(Section(entries))
    }

    fn value(&mut self, ty: u8, depth: usize) -> Result<Value, Error> {
        Ok(match ty {
            T_I64 => Value::Int(i64::from_le_bytes(self.fixed()?)),
            T_I32 => Value::Int(i64::from(i32::from_le_bytes(self.fixed()?))),
            T_I16 => Value::Int(i64::from(i16::from_le_bytes(self.fixed()?))),
            T_I8 => Value::Int(i64::from(self.fixed::<1>()?[0] as i8)),
            T_U64 => Value::Uint(u64::from_le_bytes(self.fixed()?)),
            T_U32 => Value::Uint(u64::from(u32::from_le_bytes(self.fixed()?))),
            T_U16 => Value::Uint(u64::from(u16::from_le_bytes(self.fixed()?))),
            T_U8 => Value::Uint(u64::from(self.fixed::<1>()?[0])),
            T_F64 => Value::Float(f64::from_le_bytes(self.fixed()?)),
            T_STRING => {
                let n = self.count()?;
                Value::Bytes(self.take(n)?)
            }
            T_BOOL => Value::Bool(self.fixed::<1>()?[0] != 0),
            T_OBJECT => Value::Object(self.section(depth)?),
            T_ARRAY => {
                let elem = self.fixed::<1>()?[0];
                if elem & FLAG_ARRAY == 0 {
                    return Err(Error::Type(elem));
                }
                let n = self.count()?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.value(elem & !FLAG_ARRAY, depth + 1)?);
                }
                Value::Array(items)
            }
            other => return Err(Error::Type(other)),
        })
    }
}

pub fn decode(buf: Bytes) -> Result<Section, Error> {
    let mut r = Reader { buf, pos: 0 };
    if u32::from_le_bytes(r.fixed()?) != SIGNATURE_A
        || u32::from_le_bytes(r.fixed()?) != SIGNATURE_B
        || r.fixed::<1>()?[0] != VERSION
    {
        return Err(Error::Signature);
    }
    r.section(0)
}

fn put_varint(out: &mut Vec<u8>, v: u64) {
    if v <= 63 {
        out.push((v << 2) as u8);
    } else if v <= 16_383 {
        out.extend_from_slice(&(((v << 2) | 1) as u16).to_le_bytes());
    } else if v <= 1_073_741_823 {
        out.extend_from_slice(&(((v << 2) | 2) as u32).to_le_bytes());
    } else {
        out.extend_from_slice(&((v << 2) | 3).to_le_bytes());
    }
}

fn type_of(value: &Value) -> u8 {
    match value {
        Value::Int(_) => T_I64,
        Value::Uint(_) => T_U64,
        Value::Float(_) => T_F64,
        Value::Bytes(_) => T_STRING,
        Value::Bool(_) => T_BOOL,
        Value::Object(_) => T_OBJECT,
        Value::Array(_) => T_ARRAY,
    }
}

fn put_raw(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Int(v) => out.extend_from_slice(&v.to_le_bytes()),
        Value::Uint(v) => out.extend_from_slice(&v.to_le_bytes()),
        Value::Float(v) => out.extend_from_slice(&v.to_le_bytes()),
        Value::Bytes(b) => {
            put_varint(out, b.len() as u64);
            out.extend_from_slice(b);
        }
        Value::Bool(v) => out.push(*v as u8),
        Value::Object(s) => put_section(out, s),
        Value::Array(items) => {
            // Nested arrays are never sent; encode as the element type.
            let elem = items.first().map(type_of).unwrap_or(T_STRING);
            out.push(elem | FLAG_ARRAY);
            put_varint(out, items.len() as u64);
            items.iter().for_each(|i| put_raw(out, i));
        }
    }
}

fn put_section(out: &mut Vec<u8>, section: &Section) {
    put_varint(out, section.0.len() as u64);
    for (name, value) in &section.0 {
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        match value {
            Value::Array(items) => {
                let elem = items.first().map(type_of).unwrap_or(T_STRING);
                out.push(elem | FLAG_ARRAY);
                put_varint(out, items.len() as u64);
                items.iter().for_each(|i| put_raw(out, i));
            }
            other => {
                out.push(type_of(other));
                put_raw(out, other);
            }
        }
    }
}

pub fn encode(root: &Section) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&SIGNATURE_A.to_le_bytes());
    out.extend_from_slice(&SIGNATURE_B.to_le_bytes());
    out.push(VERSION);
    put_section(&mut out, root);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut inner = Section::default();
        inner.insert("indices", Value::Array(vec![Value::Uint(1), Value::Uint(u64::MAX)]));
        let mut root = Section::default();
        root.insert("start_height", Value::Uint(1_234_567));
        root.insert("prune", Value::Bool(true));
        root.insert("block_ids", Value::Bytes(Bytes::from(vec![7u8; 96])));
        root.insert("output_indices", Value::Array(vec![Value::Object(inner.clone())]));
        root.insert("status", Value::Bytes(Bytes::from_static(b"OK")));
        let decoded = decode(Bytes::from(encode(&root))).unwrap();
        assert_eq!(decoded, root);
        assert_eq!(decoded.u64("start_height").unwrap(), 1_234_567);
        assert!(decoded.array("absent").unwrap().is_empty());
    }

    #[test]
    fn varint_boundaries() {
        for v in [0u64, 63, 64, 16_383, 16_384, 1_073_741_823, 1_073_741_824, u64::MAX >> 2] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            let mut r = Reader { buf: Bytes::from(buf), pos: 0 };
            assert_eq!(r.varint().unwrap(), v);
        }
    }

    #[test]
    fn rejects_hostile_input() {
        assert_eq!(decode(Bytes::from_static(b"no")).unwrap_err(), Error::Truncated);
        assert_eq!(decode(Bytes::from_static(b"nope")).unwrap_err(), Error::Signature);
        assert_eq!(decode(Bytes::from(vec![0u8; 16])).unwrap_err(), Error::Signature);
        // Claims a 2^40-byte string.
        let mut buf = encode(&Section::default());
        buf.pop();
        buf.push(4); // one entry
        buf.extend_from_slice(&[1, b'x', T_STRING]);
        put_varint(&mut buf, 1 << 40);
        assert_eq!(decode(Bytes::from(buf)).unwrap_err(), Error::Truncated);
    }
}
