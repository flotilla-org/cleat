//! Stateful removal of session-owned OSC 7501 from raw terminal output.
//! Retains only a possible selector prefix; status payloads are discarded as they arrive.
#[derive(Default)]
pub(crate) struct StatusOutputFilter {
    prefix: Vec<u8>,
    dropping: bool,
    escaped: bool,
}
impl StatusOutputFilter {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &byte in bytes {
            if self.dropping {
                if byte == 7 || byte == 0x9c || self.escaped && byte == b'\\' || matches!(byte, 0x18 | 0x1a) {
                    self.dropping = false;
                    self.escaped = false;
                } else {
                    self.escaped = byte == 0x1b;
                }
                continue;
            }
            self.prefix.push(byte);
            let candidates: [&[u8]; 2] = [b"\x1b]7501;", b"\x9d7501;"];
            if candidates.iter().any(|candidate| *candidate == self.prefix) {
                self.prefix.clear();
                self.dropping = true;
                self.escaped = false;
            } else {
                while !self.prefix.is_empty() && !candidates.iter().any(|candidate| candidate.starts_with(&self.prefix)) {
                    out.push(self.prefix.remove(0));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // VT output drops queries and reports under every possible transport split;
    // unrelated OSCs, UTF-8 and ordinary terminal output retain their bytes.
    #[test]
    fn every_split_removes_only_status() {
        let input = b"a\x1b]7501;?\x1b\\b\x1b]7501;state=done\x07c\x1b]0;title\x07\x1b[2J";
        for split in 0..=input.len() {
            let mut filter = StatusOutputFilter::default();
            let mut actual = filter.push(&input[..split]);
            actual.extend(filter.push(&input[split..]));
            assert_eq!(actual, b"abc\x1b]0;title\x07\x1b[2J");
        }
    }
}
