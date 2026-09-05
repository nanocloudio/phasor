//! Bytes of ASCII text into a bounded buffer.
//!
//! The edge renders numbers and names into byte buffers in several places:
//! a probe's report, a diagnostic line, an oracle's summary. Each is a
//! bounded write that stops at the buffer's end rather than failing, which is
//! what these two helpers do, once.

/// Copy `text` into the front of `out`, answering how many bytes fit.
pub fn put_ascii(out: &mut [u8], text: &[u8]) -> usize {
    let length = text.len().min(out.len());
    if let (Some(dest), Some(src)) = (out.get_mut(..length), text.get(..length)) {
        dest.copy_from_slice(src);
    }
    length
}

/// Write `value` in decimal into the front of `out`, answering how many
/// bytes fit.
pub fn put_u32(out: &mut [u8], value: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut count = 0usize;
    let mut rest = value;
    loop {
        digits[count] = b'0' + u8::try_from(rest % 10).unwrap_or(0);
        count += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    let mut written = 0usize;
    while count > 0 && written < out.len() {
        count -= 1;
        out[written] = digits[count];
        written += 1;
    }
    written
}
