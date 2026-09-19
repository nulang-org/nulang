//! Minimal allocation-free RESP2 command parser.
//!
//! The parser validates the entire command frame but stores only borrowed
//! slices into the caller's network buffer. Arguments are exposed through a
//! lazy iterator, so common commands such as GET/SET do not allocate a Vec of
//! argument objects before routing to the shard-local cache kernel.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespParseError {
    ExpectedArray,
    ExpectedBulkString,
    InvalidInteger,
    InvalidLength,
    InvalidTerminator,
    EmptyCommand,
}

#[derive(Debug, Clone, Copy)]
pub struct RespCommand<'a> {
    name: &'a [u8],
    args_bytes: &'a [u8],
    argc: usize,
}

impl<'a> RespCommand<'a> {
    pub fn name(&self) -> &'a [u8] {
        self.name
    }

    pub fn argc(&self) -> usize {
        self.argc
    }

    pub fn args(&self) -> RespArgs<'a> {
        RespArgs {
            bytes: self.args_bytes,
            cursor: 0,
            remaining: self.argc,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RespArgs<'a> {
    bytes: &'a [u8],
    cursor: usize,
    remaining: usize,
}

impl<'a> Iterator for RespArgs<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let (value, consumed) = parse_bulk(&self.bytes[self.cursor..]).ok()??;
        self.cursor += consumed;
        self.remaining -= 1;
        Some(value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for RespArgs<'_> {}

/// Parse one RESP2 array-of-bulk-strings command.
///
/// `Ok(None)` means the caller should read more bytes. On success the returned
/// usize is the exact number of consumed bytes, allowing pipelined frames to
/// remain in the socket buffer.
pub fn parse_command(input: &[u8]) -> Result<Option<(RespCommand<'_>, usize)>, RespParseError> {
    if input.is_empty() {
        return Ok(None);
    }
    if input[0] != b'*' {
        return Err(RespParseError::ExpectedArray);
    }

    let Some((count, mut cursor)) = parse_number_line(input, b'*')? else {
        return Ok(None);
    };
    if count <= 0 {
        return Err(RespParseError::EmptyCommand);
    }
    let count = count as usize;

    let Some((name, consumed)) = parse_bulk(&input[cursor..])? else {
        return Ok(None);
    };
    cursor += consumed;
    let args_start = cursor;

    for _ in 1..count {
        let Some((_, consumed)) = parse_bulk(&input[cursor..])? else {
            return Ok(None);
        };
        cursor += consumed;
    }

    Ok(Some((
        RespCommand {
            name,
            args_bytes: &input[args_start..cursor],
            argc: count - 1,
        },
        cursor,
    )))
}

fn parse_bulk(input: &[u8]) -> Result<Option<(&[u8], usize)>, RespParseError> {
    if input.is_empty() {
        return Ok(None);
    }
    if input[0] != b'$' {
        return Err(RespParseError::ExpectedBulkString);
    }

    let Some((len, header)) = parse_number_line(input, b'$')? else {
        return Ok(None);
    };
    if len < 0 {
        return Err(RespParseError::InvalidLength);
    }
    let len = len as usize;
    let end = header
        .checked_add(len)
        .ok_or(RespParseError::InvalidLength)?;
    let frame_end = end
        .checked_add(2)
        .ok_or(RespParseError::InvalidLength)?;
    if input.len() < frame_end {
        return Ok(None);
    }
    if &input[end..frame_end] != b"\r\n" {
        return Err(RespParseError::InvalidTerminator);
    }
    Ok(Some((&input[header..end], frame_end)))
}

fn parse_number_line(
    input: &[u8],
    marker: u8,
) -> Result<Option<(i64, usize)>, RespParseError> {
    if input.is_empty() {
        return Ok(None);
    }
    if input[0] != marker {
        return Err(RespParseError::InvalidInteger);
    }

    let Some(line_end) = input[1..].windows(2).position(|window| window == b"\r\n") else {
        return Ok(None);
    };
    let line_end = line_end + 1;
    let digits = &input[1..line_end];
    if digits.is_empty() {
        return Err(RespParseError::InvalidInteger);
    }

    let mut negative = false;
    let mut idx = 0usize;
    if digits[0] == b'-' {
        negative = true;
        idx = 1;
        if digits.len() == 1 {
            return Err(RespParseError::InvalidInteger);
        }
    }

    let mut value = 0i64;
    while idx < digits.len() {
        let digit = digits[idx];
        if !digit.is_ascii_digit() {
            return Err(RespParseError::InvalidInteger);
        }
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add((digit - b'0') as i64))
            .ok_or(RespParseError::InvalidInteger)?;
        idx += 1;
    }
    if negative {
        value = value.checked_neg().ok_or(RespParseError::InvalidInteger)?;
    }
    Ok(Some((value, line_end + 2)))
}

pub fn write_simple(out: &mut Vec<u8>, value: &[u8]) {
    out.push(b'+');
    out.extend_from_slice(value);
    out.extend_from_slice(b"\r\n");
}

pub fn write_error(out: &mut Vec<u8>, value: &[u8]) {
    out.push(b'-');
    out.extend_from_slice(value);
    out.extend_from_slice(b"\r\n");
}

pub fn write_integer(out: &mut Vec<u8>, value: i64) {
    out.push(b':');
    write_i64_decimal(out, value);
    out.extend_from_slice(b"\r\n");
}

pub fn write_bulk(out: &mut Vec<u8>, value: &[u8]) {
    out.push(b'

pub fn write_null_bulk(out: &mut Vec<u8>) {
    out.extend_from_slice(b"$-1\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_get_without_argument_allocation() {
        let input = b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n";
        let (command, consumed) = parse_command(input).unwrap().unwrap();
        assert_eq!(consumed, input.len());
        assert_eq!(command.name(), b"GET");
        assert_eq!(command.argc(), 1);
        assert_eq!(command.args().collect::<Vec<_>>(), vec![b"foo".as_slice()]);
    }

    #[test]
    fn parses_pipelined_command_boundary() {
        let first = b"*1\r\n$4\r\nPING\r\n";
        let mut input = first.to_vec();
        input.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$1\r\nx\r\n");
        let (command, consumed) = parse_command(&input).unwrap().unwrap();
        assert_eq!(command.name(), b"PING");
        assert_eq!(consumed, first.len());
    }

    #[test]
    fn incomplete_frame_requests_more_bytes() {
        assert_eq!(parse_command(b"*2\r\n$3\r\nGET\r\n$5\r\nhe").unwrap(), None);
    }

    #[test]
    fn rejects_non_bulk_command_arguments() {
        assert_eq!(
            parse_command(b"*2\r\n$3\r\nGET\r\n:1\r\n"),
            Err(RespParseError::ExpectedBulkString)
        );
    }

    #[test]
    fn encodes_basic_responses() {
        let mut out = Vec::new();
        write_bulk(&mut out, b"hello");
        assert_eq!(out, b"$5\r\nhello\r\n");
        out.clear();
        write_integer(&mut out, 42);
        assert_eq!(out, b":42\r\n");
    }
}
);
    write_u64_decimal(out, value.len() as u64);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(value);
    out.extend_from_slice(b"\r\n");
}

fn write_i64_decimal(out: &mut Vec<u8>, value: i64) {
    if value < 0 {
        out.push(b'-');
    }
    write_u64_decimal(out, value.unsigned_abs());
}

fn write_u64_decimal(out: &mut Vec<u8>, mut value: u64) {
    let mut buf = [0u8; 20];
    let mut cursor = buf.len();

    if value == 0 {
        out.push(b'0');
        return;
    }

    while value != 0 {
        cursor -= 1;
        buf[cursor] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    out.extend_from_slice(&buf[cursor..]);
}

pub fn write_null_bulk(out: &mut Vec<u8>) {
    out.extend_from_slice(b"$-1\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_get_without_argument_allocation() {
        let input = b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n";
        let (command, consumed) = parse_command(input).unwrap().unwrap();
        assert_eq!(consumed, input.len());
        assert_eq!(command.name(), b"GET");
        assert_eq!(command.argc(), 1);
        assert_eq!(command.args().collect::<Vec<_>>(), vec![b"foo".as_slice()]);
    }

    #[test]
    fn parses_pipelined_command_boundary() {
        let first = b"*1\r\n$4\r\nPING\r\n";
        let mut input = first.to_vec();
        input.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$1\r\nx\r\n");
        let (command, consumed) = parse_command(&input).unwrap().unwrap();
        assert_eq!(command.name(), b"PING");
        assert_eq!(consumed, first.len());
    }

    #[test]
    fn incomplete_frame_requests_more_bytes() {
        assert_eq!(parse_command(b"*2\r\n$3\r\nGET\r\n$5\r\nhe").unwrap(), None);
    }

    #[test]
    fn rejects_non_bulk_command_arguments() {
        assert_eq!(
            parse_command(b"*2\r\n$3\r\nGET\r\n:1\r\n"),
            Err(RespParseError::ExpectedBulkString)
        );
    }

    #[test]
    fn encodes_basic_responses() {
        let mut out = Vec::new();
        write_bulk(&mut out, b"hello");
        assert_eq!(out, b"$5\r\nhello\r\n");
        out.clear();
        write_integer(&mut out, 42);
        assert_eq!(out, b":42\r\n");
    }
}
