//! Strict, error-reporting UTF-8 char decoding from [`BufRead`] streams.
//!
//! Every sequence is validated and the offending bytes are reported. Bytes are
//! consumed only for sequences that are yielded, so an invalid continuation
//! byte is left in the stream because it may start the next char — matching
//! upstream `xutf`.

use core::{fmt, marker::PhantomData};
use std::{
    error::Error,
    io::{self, BufRead, ErrorKind},
};

use crate::Utf8;

/// Bytes read from a stream that failed to decode, plus the [`io::Error`]
/// classifying the failure.
///
/// The kind is [`InvalidData`](ErrorKind::InvalidData) for a malformed
/// sequence, [`UnexpectedEof`](ErrorKind::UnexpectedEof) for a sequence cut
/// short by end of stream, or a real I/O error propagated from the reader.
#[derive(Debug)]
pub struct ReadCharError {
    bytes: [u8; 4],
    len: u8,
    io_error: io::Error,
}

impl ReadCharError {
    fn new(bytes: &[u8], io_error: io::Error) -> Self {
        let mut store = [0u8; 4];
        let len = bytes.len().min(store.len());
        store[..len].copy_from_slice(&bytes[..len]);
        Self {
            bytes: store,
            len: u8::try_from(len).expect("buf holds at most 4 bytes"),
            io_error,
        }
    }

    fn invalid(bytes: &[u8]) -> Self {
        Self::new(bytes, ErrorKind::InvalidData.into())
    }

    fn eof(bytes: &[u8]) -> Self {
        Self::new(bytes, ErrorKind::UnexpectedEof.into())
    }

    /// The raw bytes, in stream order, of the invalid or incomplete sequence.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// The I/O error classifying this failure.
    #[must_use]
    pub const fn as_io_error(&self) -> &io::Error {
        &self.io_error
    }

    /// Converts into an [`io::Error`], keeping the byte context as the payload
    /// (and `self` reachable through [`Error::source`]).
    #[must_use]
    pub fn into_io_error(self) -> io::Error {
        if self.len == 0 {
            return self.io_error;
        }
        io::Error::new(self.io_error.kind(), self)
    }
}

impl Error for ReadCharError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.io_error)
    }
}

impl From<ReadCharError> for io::Error {
    fn from(e: ReadCharError) -> Self {
        e.into_io_error()
    }
}

impl fmt::Display for ReadCharError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid byte sequence")?;
        for b in self.as_bytes() {
            write!(f, " {b:02X}")?;
        }
        write!(f, " read")?;
        match self.io_error.kind() {
            ErrorKind::InvalidData => Ok(()),
            ErrorKind::UnexpectedEof => write!(f, " (unexpected EOF)"),
            _ => write!(f, " ({})", self.io_error),
        }
    }
}

/// Peeks the next byte without consuming it; `Ok(None)` at end of stream.
fn peek_byte<R: BufRead + ?Sized>(reader: &mut R) -> io::Result<Option<u8>> {
    loop {
        match reader.fill_buf() {
            Ok(buf) => return Ok(buf.first().copied()),
            Err(e) if e.kind() == ErrorKind::Interrupted => (),
            Err(e) => return Err(e),
        }
    }
}

/// Consumes one byte; `Ok(None)` at end of stream.
fn take_byte<R: BufRead + ?Sized>(reader: &mut R) -> io::Result<Option<u8>> {
    loop {
        let byte = {
            let buf = match reader.fill_buf() {
                Ok(buf) => buf,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            match buf.first() {
                Some(&byte) => byte,
                None => return Ok(None),
            }
        };
        reader.consume(1);
        return Ok(Some(byte));
    }
}

/// Decodes one UTF-8 scalar, or reports the bytes that failed to decode.
pub(crate) fn next_raw<R: BufRead + ?Sized>(reader: &mut R) -> Option<Result<char, ReadCharError>> {
    let first = match take_byte(reader) {
        Ok(Some(byte)) => byte,
        Ok(None) => return None,
        Err(e) => return Some(Err(ReadCharError::new(&[], e))),
    };
    if first < 0x80 {
        return Some(Ok(char::from(first)));
    }

    let needed = match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        // Continuation byte, overlong lead (`C0`/`C1`), or out of range (`F5..`).
        _ => return Some(Err(ReadCharError::invalid(&[first]))),
    };

    let mut buf = [0u8; 4];
    buf[0] = first;
    let mut len = 1usize;
    while len < needed {
        let peeked = match peek_byte(reader) {
            Ok(peeked) => peeked,
            Err(e) => return Some(Err(ReadCharError::new(&buf[..len], e))),
        };
        let Some(byte) = peeked else {
            return Some(Err(ReadCharError::eof(&buf[..len])));
        };
        if !(0x80..=0xbf).contains(&byte) {
            // Leave the offending byte in the stream: it may start the next char.
            return Some(Err(ReadCharError::invalid(&buf[..len])));
        }
        match take_byte(reader) {
            Ok(Some(_)) => {
                buf[len] = byte;
                len += 1;
            }
            Ok(None) => return Some(Err(ReadCharError::eof(&buf[..len]))),
            Err(e) => return Some(Err(ReadCharError::new(&buf[..len], e))),
        }
    }

    // Rejects overlongs, surrogates and scalars past U+10FFFF.
    match core::str::from_utf8(&buf[..needed]) {
        Ok(text) => Some(Ok(text.chars().next().expect("one scalar decoded"))),
        Err(_) => Some(Err(ReadCharError::invalid(&buf[..needed]))),
    }
}

/// Char iterator over a [`BufRead`]; yields [`io::Result<char>`] like upstream.
pub struct Chars<'a, R: BufRead + ?Sized, E = Utf8>(CharsRaw<'a, R, E>);

/// Char iterator yielding [`ReadCharError`] directly.
pub struct CharsRaw<'a, R: BufRead + ?Sized, E = Utf8> {
    reader: &'a mut R,
    _encoding: PhantomData<E>,
}

impl<R: BufRead + ?Sized, E> Iterator for Chars<'_, R, E> {
    type Item = io::Result<char>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|r| r.map_err(io::Error::from))
    }
}

impl<R: BufRead + ?Sized, E> Iterator for CharsRaw<'_, R, E> {
    type Item = Result<char, ReadCharError>;

    fn next(&mut self) -> Option<Self::Item> {
        next_raw(self.reader)
    }
}

/// Extension trait adding char iteration to every [`BufRead`].
pub trait BufReadCharsExt: BufRead {
    /// Iterates the UTF-8 chars of this reader as [`io::Result<char>`].
    fn chars(&mut self) -> Chars<'_, Self> {
        self.decode_chars()
    }

    /// Iterates the UTF-8 chars, with errors carrying the offending bytes.
    fn chars_raw(&mut self) -> CharsRaw<'_, Self> {
        self.decode_chars_raw()
    }

    /// Iterates the chars of this reader in encoding `E` as [`io::Result<char>`].
    fn decode_chars<E>(&mut self) -> Chars<'_, Self, E> {
        Chars(self.decode_chars_raw())
    }

    /// Iterates the chars in encoding `E`, yielding [`ReadCharError`] directly.
    fn decode_chars_raw<E>(&mut self) -> CharsRaw<'_, Self, E> {
        CharsRaw {
            reader: self,
            _encoding: PhantomData,
        }
    }

    /// Reads one char; `Ok(None)` at end of stream.
    ///
    /// # Errors
    ///
    /// Returns the malformed sequence as an [`io::Error`] of kind
    /// [`InvalidData`](ErrorKind::InvalidData) or
    /// [`UnexpectedEof`](ErrorKind::UnexpectedEof), or a real I/O error from the
    /// reader.
    fn read_char(&mut self) -> io::Result<Option<char>> {
        self.read_char_raw().map_err(io::Error::from)
    }

    /// Reads one char, with errors carrying the offending bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ReadCharError`] for a malformed or truncated sequence, or for a
    /// real I/O error from the reader.
    fn read_char_raw(&mut self) -> Result<Option<char>, ReadCharError> {
        next_raw(self).transpose()
    }
}

impl<R: BufRead + ?Sized> BufReadCharsExt for R {}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, ErrorKind};

    use super::BufReadCharsExt;

    fn decode(input: &[u8]) -> (String, Vec<(Vec<u8>, ErrorKind)>) {
        let mut reader = BufReader::new(input);
        let mut text = String::new();
        let mut errors = Vec::new();
        for item in reader.chars_raw() {
            match item {
                Ok(ch) => text.push(ch),
                Err(e) => errors.push((e.as_bytes().to_vec(), e.as_io_error().kind())),
            }
        }
        (text, errors)
    }

    #[test]
    fn decodes_multibyte_text() {
        assert_eq!(
            decode("caf\u{e9} \u{754c} \u{1f600}".as_bytes()),
            ("caf\u{e9} \u{754c} \u{1f600}".to_owned(), Vec::new(),)
        );
    }

    #[test]
    fn decodes_across_buffer_refills() {
        let mut reader = BufReader::with_capacity(1, "\u{754c}\u{e9}".as_bytes());
        let chars: Result<Vec<char>, _> = reader.chars().collect();
        assert_eq!(chars.expect("valid utf-8"), vec!['\u{754c}', '\u{e9}']);
    }

    #[test]
    fn reports_invalid_lead_and_continuation_bytes() {
        let (text, errors) = decode(b"a\xffb");
        assert_eq!(text, "ab");
        assert_eq!(errors, vec![(vec![0xff], ErrorKind::InvalidData)]);

        // An overlong lead is rejected one byte at a time; the stray
        // continuation byte is reported next, not swallowed with it.
        let (text, errors) = decode(b"\xc0\x80");
        assert_eq!(text, "");
        assert_eq!(
            errors,
            vec![
                (vec![0xc0], ErrorKind::InvalidData),
                (vec![0x80], ErrorKind::InvalidData)
            ]
        );

        // Surrogate half: valid framing, illegal scalar.
        let (_text, errors) = decode(b"\xed\xa0\x80");
        assert_eq!(
            errors,
            vec![(vec![0xed, 0xa0, 0x80], ErrorKind::InvalidData)]
        );
    }

    #[test]
    fn reports_truncated_sequences_as_unexpected_eof() {
        let (_text, errors) = decode(b"\xe4\xb8");
        assert_eq!(errors, vec![(vec![0xe4, 0xb8], ErrorKind::UnexpectedEof)]);
    }

    #[test]
    fn read_char_reports_none_at_end_of_stream() {
        let mut reader = BufReader::new("ab".as_bytes());
        assert_eq!(reader.read_char().expect("ok"), Some('a'));
        assert_eq!(reader.read_char().expect("ok"), Some('b'));
        assert_eq!(reader.read_char().expect("ok"), None);
    }

    #[test]
    fn io_error_keeps_byte_context_as_payload() {
        let mut reader = BufReader::new(b"\xff".as_slice());
        let err = reader.read_char().expect_err("invalid");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        let inner = err
            .get_ref()
            .and_then(|e| e.downcast_ref::<super::ReadCharError>());
        assert_eq!(inner.expect("payload").as_bytes(), [0xff]);
    }
}
