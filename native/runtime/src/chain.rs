//! Agent-to-agent chain budget. Rust owns it with the rest of the conversation state, so an
//! agent can never talk its way past a limit: the host only delivers prompts the engine emits.
use std::time::{Duration, Instant};

/// Agent turns allowed per user turn, counting the one that answered the user.
pub const MAX_AGENT_TURNS: u32 = 4;
/// Wall-clock cap from the moment the user's turn was committed.
pub const MAX_WALL: Duration = Duration::from_secs(90);
/// Consecutive hand-offs between the same two agents (two round trips).
pub const MAX_PAIR_HOPS: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub turns: u32,
    pub wall: Duration,
    pub pair_hops: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            turns: MAX_AGENT_TURNS,
            wall: MAX_WALL,
            pair_hops: MAX_PAIR_HOPS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EndReason {
    Turns,
    Time,
    PingPong,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turns => "turns",
            Self::Time => "time",
            Self::PingPong => "ping_pong",
        }
    }
}

#[derive(Debug)]
pub struct Chain {
    pub started: Instant,
    limits: Limits,
    /// Agent turns so far, including the one that answered the user.
    turns: u32,
    /// The unordered pair of the latest hand-offs, and how many in a row were between them.
    pair: Option<(usize, usize)>,
    pair_hops: u32,
}

impl Chain {
    pub fn new(now: Instant, limits: Limits) -> Self {
        Self {
            started: now,
            limits,
            turns: 1,
            pair: None,
            pair_hops: 0,
        }
    }

    /// Admit one hand-off `from` -> `to`, or say why the chain must stop. A refused hand-off
    /// changes nothing.
    pub fn admit(&mut self, now: Instant, from: usize, to: usize) -> Result<u32, EndReason> {
        if now.saturating_duration_since(self.started) >= self.limits.wall {
            return Err(EndReason::Time);
        }
        if self.turns >= self.limits.turns {
            return Err(EndReason::Turns);
        }
        let pair = (from.min(to), from.max(to));
        let hops = if self.pair == Some(pair) {
            self.pair_hops + 1
        } else {
            1
        };
        if hops > self.limits.pair_hops {
            return Err(EndReason::PingPong);
        }
        self.pair = Some(pair);
        self.pair_hops = hops;
        self.turns += 1;
        Ok(self.turns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(limits: Limits) -> (Chain, Instant) {
        let now = Instant::now();
        (Chain::new(now, limits), now)
    }

    #[test]
    fn four_agent_turns_per_user_turn() {
        let (mut c, now) = chain(Limits::default());
        assert_eq!(c.admit(now, 0, 1), Ok(2));
        assert_eq!(c.admit(now, 1, 2), Ok(3));
        assert_eq!(c.admit(now, 2, 0), Ok(4));
        assert_eq!(c.admit(now, 0, 1), Err(EndReason::Turns));
    }

    #[test]
    fn the_wall_clock_cap_ends_the_chain() {
        let (mut c, now) = chain(Limits::default());
        assert_eq!(c.admit(now + MAX_WALL, 0, 1), Err(EndReason::Time));
        assert!(c
            .admit(now + MAX_WALL - Duration::from_secs(1), 0, 1)
            .is_ok());
    }

    #[test]
    fn ping_pong_stops_after_two_round_trips() {
        let limits = Limits {
            turns: 99,
            ..Limits::default()
        };
        let (mut c, now) = chain(limits);
        for (from, to) in [(0, 1), (1, 0), (0, 1), (1, 0)] {
            assert!(c.admit(now, from, to).is_ok());
        }
        assert_eq!(c.admit(now, 0, 1), Err(EndReason::PingPong));
        // A third agent breaks the streak.
        assert!(c.admit(now, 0, 2).is_ok());
    }

    #[test]
    fn a_refused_hand_off_changes_nothing() {
        let (mut c, now) = chain(Limits {
            turns: 2,
            ..Limits::default()
        });
        assert!(c.admit(now, 0, 1).is_ok());
        assert_eq!(c.admit(now, 1, 0), Err(EndReason::Turns));
        assert_eq!(c.turns, 2);
    }
}
