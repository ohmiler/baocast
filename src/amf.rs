//! The small subset of AMF0 (Action Message Format) that RTMP publishing needs:
//! enough to send connect/publish commands and read the server's replies.

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Bool(bool),
    String(String),
    Object(Vec<(String, Value)>),
    /// Like an object, used for onMetaData.
    EcmaArray(Vec<(String, Value)>),
    Null,
}

impl Value {
    pub fn str(text: &str) -> Self {
        Value::String(text.to_string())
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// A property of an object or ECMA array.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(props) | Value::EcmaArray(props) => props.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

pub fn encode(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        encode_value(&mut out, value);
    }
    out
}

fn encode_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Number(n) => {
            out.push(0x00);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::Bool(b) => out.extend_from_slice(&[0x01, *b as u8]),
        Value::String(text) => {
            out.push(0x02);
            encode_key(out, text);
        }
        Value::Object(props) => {
            out.push(0x03);
            encode_props(out, props);
        }
        Value::EcmaArray(props) => {
            out.push(0x08);
            out.extend_from_slice(&(props.len() as u32).to_be_bytes());
            encode_props(out, props);
        }
        Value::Null => out.push(0x05),
    }
}

fn encode_key(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u16).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn encode_props(out: &mut Vec<u8>, props: &[(String, Value)]) {
    for (key, value) in props {
        encode_key(out, key);
        encode_value(out, value);
    }
    out.extend_from_slice(&[0, 0, 0x09]); // empty key + object-end marker
}

/// Decodes as many values as it understands; stops quietly at anything else.
pub fn decode(data: &[u8]) -> Vec<Value> {
    let mut cursor = Cursor { data, pos: 0 };
    let mut values = Vec::new();
    while cursor.pos < data.len() {
        match cursor.value() {
            Some(value) => values.push(value),
            None => break,
        }
    }
    values
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let bytes = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(bytes)
    }

    fn u16(&mut self) -> Option<usize> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?) as usize)
    }

    fn u32(&mut self) -> Option<usize> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?) as usize)
    }

    fn string(&mut self, len: usize) -> Option<String> {
        Some(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    fn props(&mut self) -> Option<Vec<(String, Value)>> {
        let mut props = Vec::new();
        loop {
            let len = self.u16()?;
            if len == 0 && self.data.get(self.pos) == Some(&0x09) {
                self.pos += 1;
                return Some(props);
            }
            let key = self.string(len)?;
            props.push((key, self.value()?));
        }
    }

    fn value(&mut self) -> Option<Value> {
        let marker = *self.take(1)?.first()?;
        Some(match marker {
            0x00 => Value::Number(f64::from_be_bytes(self.take(8)?.try_into().ok()?)),
            0x01 => Value::Bool(self.take(1)?[0] != 0),
            0x02 => {
                let len = self.u16()?;
                Value::String(self.string(len)?)
            }
            0x03 => Value::Object(self.props()?),
            0x05 | 0x06 => Value::Null, // null, undefined
            0x08 => {
                self.u32()?; // count hint, not reliable
                Value::EcmaArray(self.props()?)
            }
            0x0A => {
                let count = self.u32()?;
                // Strict arrays don't appear in replies we care about; keep the values flat.
                let mut items = Vec::with_capacity(count.min(64));
                for _ in 0..count {
                    items.push((String::new(), self.value()?));
                }
                Value::EcmaArray(items)
            }
            0x0C => {
                let len = self.u32()?;
                Value::String(self.string(len)?)
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_connect_command() {
        let values = vec![
            Value::str("connect"),
            Value::Number(1.0),
            Value::Object(vec![("app".into(), Value::str("live2")), ("fpad".into(), Value::Bool(false))]),
            Value::Null,
        ];
        assert_eq!(decode(&encode(&values)), values);
    }

    #[test]
    fn reads_server_status() {
        let reply = encode(&[
            Value::str("onStatus"),
            Value::Number(0.0),
            Value::Null,
            Value::Object(vec![
                ("level".into(), Value::str("status")),
                ("code".into(), Value::str("NetStream.Publish.Start")),
            ]),
        ]);
        let values = decode(&reply);
        assert_eq!(values[3].get("code").and_then(Value::as_str), Some("NetStream.Publish.Start"));
    }
}
