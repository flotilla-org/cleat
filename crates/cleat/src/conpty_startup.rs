//! The bundled ConPTY's startup handshake (ADR 0006).
//!
//! When the program connects, OpenConsole writes `CSI 1 t` (window visible)
//! and a DA1 query (`CSI c`) ahead of any program output, then holds the
//! program's console connection until the host answers DA1 or 3 s pass.
//! These are questions to Cleat as the pseudoconsole's host, not output from
//! the program, so Cleat consumes them and answers from its VT engine whether
//! or not a client is attached. They never reach clients, recordings or the
//! engine's screen, so an attached terminal can neither answer twice nor act
//! on them.

/// The DA1 query ConPTY sends; also fed to the engine to obtain its answer.
pub(crate) const DA1_QUERY: &[u8] = b"\x1b[c";
const WINDOW_VISIBILITY: [&[u8]; 2] = [b"\x1b[1t", b"\x1b[2t"];

#[derive(Debug, Default)]
pub(crate) struct ConptyStartup {
    held: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StartupStep {
    /// The bytes so far may still be the handshake; hold them.
    Pending,
    /// The handshake ended with the DA1 query, which must now be answered.
    /// `rest` is ordinary output that followed it.
    Answered { rest: Vec<u8> },
    /// The stream did not open with the handshake; pass `bytes` on unchanged.
    Absent { bytes: Vec<u8> },
}

impl ConptyStartup {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> StartupStep {
        self.held.extend_from_slice(bytes);
        let mut at = 0;
        loop {
            let rest = &self.held[at..];
            if let Some(visibility) = WINDOW_VISIBILITY.iter().find(|sequence| rest.starts_with(sequence)) {
                at += visibility.len();
                continue;
            }
            if rest.starts_with(DA1_QUERY) {
                let rest = self.held.split_off(at + DA1_QUERY.len());
                self.held.clear();
                return StartupStep::Answered { rest };
            }
            if DA1_QUERY.starts_with(rest) || WINDOW_VISIBILITY.iter().any(|sequence| sequence.starts_with(rest)) {
                return StartupStep::Pending;
            }
            return StartupStep::Absent { bytes: std::mem::take(&mut self.held) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumes_the_bundled_greeting_and_keeps_following_output() {
        let mut startup = ConptyStartup::new();
        assert_eq!(startup.push(b"\x1b[1t\x1b[c\x1b[?1004h\x1b[?9001hhello"), StartupStep::Answered {
            rest: b"\x1b[?1004h\x1b[?9001hhello".to_vec()
        });
    }

    #[test]
    fn holds_a_greeting_split_across_reads() {
        let mut startup = ConptyStartup::new();
        assert_eq!(startup.push(b""), StartupStep::Pending);
        assert_eq!(startup.push(b"\x1b"), StartupStep::Pending);
        assert_eq!(startup.push(b"[1"), StartupStep::Pending);
        assert_eq!(startup.push(b"t\x1b["), StartupStep::Pending);
        assert_eq!(startup.push(b"c"), StartupStep::Answered { rest: Vec::new() });
    }

    #[test]
    fn answers_a_bare_da1_query() {
        let mut startup = ConptyStartup::new();
        assert_eq!(startup.push(b"\x1b[cok"), StartupStep::Answered { rest: b"ok".to_vec() });
    }

    #[test]
    fn passes_other_openings_through_unchanged() {
        let mut startup = ConptyStartup::new();
        assert_eq!(startup.push(b"\x1b[1t"), StartupStep::Pending);
        assert_eq!(startup.push(b"\x1b[?9001h"), StartupStep::Absent { bytes: b"\x1b[1t\x1b[?9001h".to_vec() });

        let mut startup = ConptyStartup::new();
        assert_eq!(startup.push(b"\x1b[0c"), StartupStep::Absent { bytes: b"\x1b[0c".to_vec() });
    }
}
