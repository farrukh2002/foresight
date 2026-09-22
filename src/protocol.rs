use std::io::{self, BufRead, Write};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub kind: String,
    pub cwd: String,
    pub session: String,
    pub payload: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Reply {
    Text(String),
    List(Vec<String>),
}

pub type Response = Result<Reply, String>;

pub fn write_json<W: Write, T: Serialize>(mut out: W, value: &T) -> io::Result<()> {
    serde_json::to_writer(&mut out, value).map_err(io::Error::other)?;
    out.write_all(b"\n")?;
    out.flush()
}

pub fn read_json<R: BufRead, T: serde::de::DeserializeOwned>(input: &mut R) -> io::Result<T> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 || !line.ends_with('\n') {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "socket closed before a complete reply"));
    }
    serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn read_nul<R: BufRead>(input: &mut R) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    if input.read_until(0, &mut bytes)? == 0 { return Ok(None); }
    if bytes.pop() != Some(0) {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unterminated stream field"));
    }
    String::from_utf8(bytes).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_and_nul_preserve_multiline_tabs_and_trailing_space() {
        let text = "first\nsecond\t  \n";
        let request = Request { kind: "T".into(), cwd: text.into(), session: text.into(), payload: text.into() };
        let mut wire = Vec::new();
        write_json(&mut wire, &request).unwrap();
        let decoded: Request = read_json(&mut wire.as_slice()).unwrap();
        assert_eq!(decoded.payload, text);
        assert_eq!(decoded.cwd, text);
        assert_eq!(decoded.session, text);
        let response: Response = Ok(Reply::List(vec![text.into(), String::new()]));
        wire.clear();
        write_json(&mut wire, &response).unwrap();
        assert_eq!(read_json::<_, Response>(&mut wire.as_slice()).unwrap(), response);
        let framed = format!("{text}\0\0");
        let mut input = framed.as_bytes();
        assert_eq!(read_nul(&mut input).unwrap(), Some(text.into()));
        assert_eq!(read_nul(&mut input).unwrap(), Some(String::new()));
        assert_eq!(read_nul(&mut input).unwrap(), None);
    }

    #[test]
    fn eof_is_not_an_empty_prediction() {
        assert_eq!(read_json::<_, Response>(&mut &b""[..]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(read_nul(&mut &b"partial"[..]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert!(read_json::<_, Response>(&mut &b"{\"Ok\":\"\"}"[..]).is_err());
    }
}
