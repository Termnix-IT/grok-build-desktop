#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trip {
    MaxTurns { limit: u32 },
    SameError { count: u32 },
}

pub struct Breaker {
    max_turns: u32,
    same_error_limit: u32,
    turns: u32,
    last_signature: Option<String>,
    repeat: u32,
}

impl Breaker {
    pub fn new(max_turns: u32, same_error_limit: u32) -> Self {
        Self {
            max_turns,
            same_error_limit,
            turns: 0,
            last_signature: None,
            repeat: 0,
        }
    }

    pub fn begin_turn(&mut self) -> Result<u32, Trip> {
        if self.turns >= self.max_turns {
            return Err(Trip::MaxTurns {
                limit: self.max_turns,
            });
        }
        self.turns += 1;
        Ok(self.turns)
    }

    pub fn record_failure(&mut self, signature: &str) -> Option<Trip> {
        if self.last_signature.as_deref() == Some(signature) {
            self.repeat = self.repeat.saturating_add(1);
        } else {
            self.last_signature = Some(signature.to_owned());
            self.repeat = 1;
        }
        (self.repeat >= self.same_error_limit).then_some(Trip::SameError { count: self.repeat })
    }

    pub fn record_success(&mut self) {
        self.clear_errors();
    }

    pub fn extend(&mut self, extra: u32) {
        self.max_turns = self.max_turns.saturating_add(extra);
    }

    pub fn clear_errors(&mut self) {
        self.last_signature = None;
        self.repeat = 0;
    }

    pub fn turns(&self) -> u32 {
        self.turns
    }
    pub fn max_turns(&self) -> u32 {
        self.max_turns
    }
}

pub fn trip_reason(trip: &Trip) -> String {
    match trip {
        Trip::MaxTurns { limit } => format!("ターンの上限（{limit} 回）に達しました"),
        Trip::SameError { count } => format!("同じ失敗が {count} 回続きました"),
    }
}

pub fn error_signature(source: &str, exit_code: Option<i32>, text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(5)
        .collect();
    let tail = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    let paths = normalize_paths(&tail);
    let chars: Vec<char> = paths.chars().collect();
    let mut normalized = String::new();
    let mut index = 0;
    while index < chars.len() {
        let hex_end = chars[index..]
            .iter()
            .take_while(|ch| ch.is_ascii_hexdigit())
            .count()
            + index;
        if hex_end - index >= 8 {
            normalized.push('#');
            index = hex_end;
        } else if chars[index].is_ascii_digit() {
            normalized.push('#');
            index += chars[index..]
                .iter()
                .take_while(|ch| ch.is_ascii_digit())
                .count();
        } else {
            normalized.push(chars[index]);
            index += 1;
        }
    }
    let body = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    let code = exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "none".into());
    format!("{source}|{code}|{body}")
        .chars()
        .take(300)
        .collect()
}

fn normalize_paths(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut normalized = String::new();
    let mut index = 0;
    while index < chars.len() {
        let boundary = index == 0
            || chars[index - 1].is_whitespace()
            || matches!(chars[index - 1], '\'' | '"' | '=' | '(' | '[' | '{' | ':');
        let drive = chars[index].is_ascii_alphabetic()
            && chars.get(index + 1) == Some(&':')
            && chars
                .get(index + 2)
                .is_some_and(|ch| matches!(ch, '/' | '\\'));
        let unix = chars[index] == '/' && boundary;
        let unc = chars[index] == '\\' && chars.get(index + 1) == Some(&'\\') && boundary;
        if drive || unix || unc {
            let quote = if index > 0 && matches!(chars[index - 1], '\'' | '"') {
                Some(chars[index - 1])
            } else {
                None
            };
            normalized.push_str("<path>");
            index += 1;
            while index < chars.len() {
                let ch = chars[index];
                let end = if let Some(quote) = quote {
                    ch == quote || ch == '\n' || ch == '\r'
                } else {
                    ch.is_whitespace() || matches!(ch, '\'' | '"' | '<' | '>' | ')' | ']' | '}')
                };
                if end {
                    break;
                }
                index += 1;
            }
        } else {
            normalized.push(chars[index]);
            index += 1;
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_at_limit_and_can_extend() {
        let mut breaker = Breaker::new(2, 2);
        assert_eq!(breaker.begin_turn(), Ok(1));
        assert_eq!(breaker.begin_turn(), Ok(2));
        assert_eq!(breaker.begin_turn(), Err(Trip::MaxTurns { limit: 2 }));
        assert_eq!(breaker.turns(), 2);
        breaker.extend(2);
        assert_eq!(breaker.max_turns(), 4);
        assert_eq!(breaker.begin_turn(), Ok(3));
        assert_eq!(
            trip_reason(&Trip::MaxTurns { limit: 10 }),
            "ターンの上限（10 回）に達しました"
        );
    }

    #[test]
    fn consecutive_failures_stop_and_reset() {
        let mut breaker = Breaker::new(10, 2);
        assert_eq!(breaker.record_failure("a"), None);
        assert_eq!(
            breaker.record_failure("a"),
            Some(Trip::SameError { count: 2 })
        );
        breaker.clear_errors();
        assert_eq!(breaker.record_failure("a"), None);
        assert_eq!(breaker.record_failure("b"), None);
        breaker.record_success();
        assert_eq!(breaker.record_failure("b"), None);
        assert_eq!(
            breaker.record_failure("b"),
            Some(Trip::SameError { count: 2 })
        );
        assert_eq!(
            trip_reason(&Trip::SameError { count: 2 }),
            "同じ失敗が 2 回続きました"
        );
    }

    #[test]
    fn ignores_changing_timestamps_pids_paths_and_hex() {
        let first = "2026-11-23 10:03:55 PID=12345\nfailed C:\\work\\one\\file.rs\ncache /tmp/build-123/a.out\ntrace abcdefab01234567\n  error   42  retry 2";
        let second = "2027-12-31 23:59:01 PID=9876\nfailed D:\\other\\two\\main.rs\ncache /var/cache/build-999/b.out\ntrace fedcba9876543210\nerror 199 retry 12";
        assert_eq!(
            error_signature("agent", Some(1), first),
            error_signature("agent", Some(1), second)
        );
        assert_eq!(
            error_signature("x", None, "failed \"C:\\My Project\\a.rs\" hash DEADBEEF"),
            error_signature(
                "x",
                None,
                "failed \"D:\\Other Project\\b.rs\" hash ABCDEFAB"
            )
        );
        assert_ne!(
            error_signature("a", Some(1), "error"),
            error_signature("b", Some(1), "error")
        );
        assert_ne!(
            error_signature("a", Some(1), "error"),
            error_signature("a", Some(2), "error")
        );
    }

    #[test]
    fn uses_last_five_nonempty_lines_and_limits_characters() {
        assert_eq!(
            error_signature("x", None, "old a\nold b\n\n1\n2\n\n3\n4\n5\n"),
            "x|none|# # # # #"
        );
        assert_eq!(
            error_signature("x", None, &"あ".repeat(400))
                .chars()
                .count(),
            300
        );
    }
}
