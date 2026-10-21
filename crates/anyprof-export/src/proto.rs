//! A small protobuf wire codec, enough for pprof's `profile.proto`.

pub(crate) struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub(crate) fn varint(&mut self, mut value: u64) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            self.buf.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    /// Singular proto3 field. Zero is the default and is omitted.
    pub(crate) fn field_varint(&mut self, field: u32, value: u64) {
        if value == 0 {
            return;
        }
        self.key(field, 0);
        self.varint(value);
    }

    pub(crate) fn field_bytes(&mut self, field: u32, bytes: &[u8]) {
        self.key(field, 2);
        self.varint(bytes.len() as u64);
        self.buf.extend_from_slice(bytes);
    }

    pub(crate) fn field_string(&mut self, field: u32, value: &str) {
        self.field_bytes(field, value.as_bytes());
    }

    pub(crate) fn field_message(&mut self, field: u32, nested: &Self) {
        self.field_bytes(field, &nested.buf);
    }

    /// Proto3 packed repeated numeric field. Empty slices are omitted.
    pub(crate) fn field_packed_varints(&mut self, field: u32, values: &[u64]) {
        if values.is_empty() {
            return;
        }
        let mut packed = Self::new();
        for value in values {
            packed.varint(*value);
        }
        self.field_bytes(field, &packed.buf);
    }

    fn key(&mut self, field: u32, wire: u8) {
        self.varint((u64::from(field) << 3) | u64::from(wire));
    }
}

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    index: usize,
}

pub(crate) enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, index: 0 }
    }

    pub(crate) fn next(&mut self) -> Result<Option<(u32, Value<'a>)>, String> {
        loop {
            if self.index >= self.buf.len() {
                return Ok(None);
            }
            let key = self.varint()?;
            let field = u32::try_from(key >> 3)
                .map_err(|_| "field number does not fit in u32".to_string())?;
            if field == 0 {
                return Err("field number 0".to_string());
            }
            match (key & 7) as u8 {
                0 => return Ok(Some((field, Value::Varint(self.varint()?)))),
                // fixed64 and fixed32 are unused by profiles we write; skip them.
                1 => {
                    self.take(8)?;
                }
                2 => {
                    let len = usize::try_from(self.varint()?)
                        .map_err(|_| "length does not fit in usize".to_string())?;
                    return Ok(Some((field, Value::Bytes(self.take(len)?))));
                }
                5 => {
                    self.take(4)?;
                }
                other => return Err(format!("unsupported wire type {other}")),
            }
        }
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut result = 0_u64;
        let mut shift = 0;
        loop {
            if shift >= 64 {
                return Err("varint is longer than 64 bits".to_string());
            }
            if self.index >= self.buf.len() {
                return Err("truncated varint".to_string());
            }
            let byte = self.buf[self.index];
            self.index += 1;
            let bits = u64::from(byte & 0x7f);
            if shift == 63 && bits > 1 {
                return Err("varint overflows u64".to_string());
            }
            result |= bits << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .index
            .checked_add(len)
            .ok_or_else(|| "length overflows".to_string())?;
        if end > self.buf.len() {
            return Err("truncated length-delimited field".to_string());
        }
        let slice = &self.buf[self.index..end];
        self.index = end;
        Ok(slice)
    }
}

pub(crate) fn decode_varints(bytes: &[u8]) -> Result<Vec<u64>, String> {
    let mut reader = Reader::new(bytes);
    let mut values = Vec::new();
    while reader.index < bytes.len() {
        values.push(reader.varint()?);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::{decode_varints, Reader, Writer};

    #[test]
    fn varints_roundtrip() {
        let mut writer = Writer::new();
        for value in [0, 1, 127, 128, 300, u64::from(u32::MAX)] {
            writer.varint(value);
        }
        let decoded = decode_varints(&writer.into_vec()).unwrap();
        assert_eq!(decoded, vec![0, 1, 127, 128, 300, u64::from(u32::MAX)]);
    }

    #[test]
    fn packed_field_and_empty_string_roundtrip() {
        let mut writer = Writer::new();
        writer.field_varint(1, 0);
        writer.field_packed_varints(1, &[1, 2]);
        writer.field_string(6, "");
        writer.field_string(6, "main");
        let bytes = writer.into_vec();

        let mut reader = Reader::new(&bytes);
        let (field, value) = reader.next().unwrap().unwrap();
        assert_eq!(field, 1);
        match value {
            super::Value::Bytes(packed) => assert_eq!(decode_varints(packed).unwrap(), vec![1, 2]),
            super::Value::Varint(_) => panic!("location ids should be packed"),
        }
        let (_, empty) = reader.next().unwrap().unwrap();
        match empty {
            super::Value::Bytes(text) => assert!(text.is_empty()),
            super::Value::Varint(_) => panic!("string field"),
        }
        let (_, name) = reader.next().unwrap().unwrap();
        match name {
            super::Value::Bytes(text) => assert_eq!(text, b"main"),
            super::Value::Varint(_) => panic!("string field"),
        }
        assert!(reader.next().unwrap().is_none());
    }
}
