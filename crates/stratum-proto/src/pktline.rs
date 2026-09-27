//! git pkt-line framing (kept dependency-free: the format is 4 hex digits of
//! length followed by payload; 0000 = flush, 0001 = delim).

use std::io::{Read, Write};

pub const MAX_DATA: usize = 65516;

pub fn write_data(w: &mut impl Write, data: &[u8]) -> std::io::Result<()> {
    // A payload past the pkt-line maximum would render a 5-hex-digit length
    // and corrupt the framing; make it a hard error in every build.
    if data.len() > MAX_DATA {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "pkt-line payload too large",
        ));
    }
    write!(w, "{:04x}", data.len() + 4)?;
    w.write_all(data)
}

pub fn write_text(w: &mut impl Write, line: &str) -> std::io::Result<()> {
    let mut owned;
    let data = if line.ends_with('\n') {
        line.as_bytes()
    } else {
        owned = String::with_capacity(line.len() + 1);
        owned.push_str(line);
        owned.push('\n');
        owned.as_bytes()
    };
    write_data(w, data)
}

pub fn write_flush(w: &mut impl Write) -> std::io::Result<()> {
    w.write_all(b"0000")
}

pub fn write_delim(w: &mut impl Write) -> std::io::Result<()> {
    w.write_all(b"0001")
}

#[derive(Debug, PartialEq)]
pub enum Pkt {
    Data(Vec<u8>),
    Delim,
    Flush,
    Eof,
}

pub fn read_pkt(r: &mut impl Read) -> std::io::Result<Pkt> {
    let mut len_buf = [0u8; 4];
    let mut filled = 0;
    while filled < 4 {
        let n = r.read(&mut len_buf[filled..])?;
        if n == 0 {
            if filled == 0 {
                return Ok(Pkt::Eof);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated pkt-line length",
            ));
        }
        filled += n;
    }
    let len_str = std::str::from_utf8(&len_buf)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad pkt len"))?;
    let len = usize::from_str_radix(len_str, 16)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad pkt len"))?;
    match len {
        0 => Ok(Pkt::Flush),
        1 => Ok(Pkt::Delim),
        2..=3 => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid pkt length",
        )),
        _ => {
            let mut data = vec![0u8; len - 4];
            r.read_exact(&mut data)?;
            Ok(Pkt::Data(data))
        }
    }
}

/// Read pkt payloads as trimmed text lines until the given terminator
/// (Delim or Flush); Eof also terminates.
pub fn read_section(r: &mut impl Read) -> std::io::Result<(Vec<String>, Pkt)> {
    let mut lines = Vec::new();
    loop {
        match read_pkt(r)? {
            Pkt::Data(d) => {
                let s = String::from_utf8_lossy(&d);
                lines.push(s.trim_end_matches('\n').to_string());
            }
            term => return Ok((lines, term)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_roundtrip() {
        let mut buf = Vec::new();
        write_text(&mut buf, "hello").unwrap();
        write_delim(&mut buf).unwrap();
        write_data(&mut buf, b"raw\x00bytes").unwrap();
        write_flush(&mut buf).unwrap();

        let mut r = std::io::Cursor::new(buf);
        assert_eq!(read_pkt(&mut r).unwrap(), Pkt::Data(b"hello\n".to_vec()));
        assert_eq!(read_pkt(&mut r).unwrap(), Pkt::Delim);
        assert_eq!(
            read_pkt(&mut r).unwrap(),
            Pkt::Data(b"raw\x00bytes".to_vec())
        );
        assert_eq!(read_pkt(&mut r).unwrap(), Pkt::Flush);
        assert_eq!(read_pkt(&mut r).unwrap(), Pkt::Eof);
    }

    #[test]
    fn oversized_payload_is_an_error() {
        let mut buf = Vec::new();
        assert!(write_data(&mut buf, &vec![0u8; MAX_DATA + 1]).is_err());
        assert!(buf.is_empty());
    }

    #[test]
    fn malformed_lengths_error() {
        assert!(read_pkt(&mut std::io::Cursor::new(b"zzzz".to_vec())).is_err());
        assert!(read_pkt(&mut std::io::Cursor::new(b"0002".to_vec())).is_err());
        assert!(read_pkt(&mut std::io::Cursor::new(b"00".to_vec())).is_err()); // truncated len
                                                                               // truncated payload
        assert!(read_pkt(&mut std::io::Cursor::new(b"000axy".to_vec())).is_err());
    }

    #[test]
    fn section_reader_stops_at_terminators() {
        let mut buf = Vec::new();
        write_text(&mut buf, "a").unwrap();
        write_text(&mut buf, "b").unwrap();
        write_delim(&mut buf).unwrap();
        write_text(&mut buf, "c").unwrap();
        write_flush(&mut buf).unwrap();
        let mut r = std::io::Cursor::new(buf);
        let (lines, term) = read_section(&mut r).unwrap();
        assert_eq!(lines, vec!["a", "b"]);
        assert_eq!(term, Pkt::Delim);
        let (lines, term) = read_section(&mut r).unwrap();
        assert_eq!(lines, vec!["c"]);
        assert_eq!(term, Pkt::Flush);
    }
}
