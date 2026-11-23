//! Who a thread waits on: decided from its comments alone, so the home page and any later
//! caller agree.

/// The author of every comment written from the reader page.
pub const READER: &str = "reader";

/// Whether a comment is on the reader's page or held back until Publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    Now,
    Held,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waiter {
    /// The last visible comment is an agent's: the reader's turn.
    Reader,
    /// The last visible comment is the reader's: this agent's turn.
    Agent(String),
    /// No visible comment yet.
    Nobody,
}

/// `comments_in_order` is `(author, visibility)`, oldest first. `owner` is the collection's
/// `created_by`, who is asked when the reader spoke and no agent has yet in the thread. Held
/// comments never decide the waiter; the caller counts them for "held for Publish".
pub fn waits_on(comments_in_order: &[(&str, Shown)], owner: &str) -> Waiter {
    let mut visible = comments_in_order
        .iter()
        .filter(|(_, s)| *s == Shown::Now)
        .map(|(a, _)| *a);
    let Some(last) = visible.clone().next_back() else {
        return Waiter::Nobody;
    };
    if last != READER {
        return Waiter::Reader;
    }
    let agent = visible.rfind(|a| *a != READER).unwrap_or(owner);
    Waiter::Agent(agent.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use Shown::{Held, Now};
    use margin_test_id::test_id;

    #[test_id(tdus6pi)]
    #[test]
    fn the_reader_is_waited_on_when_an_agent_spoke_last() {
        let c = [("reader", Now), ("ada", Now)];
        assert_eq!(waits_on(&c, "owner"), Waiter::Reader);
    }

    #[test_id(tajkyc0)]
    #[test]
    fn the_owner_is_waited_on_when_the_reader_spoke_alone() {
        assert_eq!(
            waits_on(&[("reader", Now)], "owner"),
            Waiter::Agent("owner".into())
        );
    }

    #[test_id(thdzq40)]
    #[test]
    fn the_most_recent_agent_is_waited_on_after_the_reader_replies() {
        let c = [("ada", Now), ("bo", Now), ("reader", Now)];
        assert_eq!(waits_on(&c, "owner"), Waiter::Agent("bo".into()));
    }

    #[test_id(tr6sfad)]
    #[test]
    fn a_thread_of_only_held_comments_waits_on_nobody() {
        let c = [("ada", Held), ("bo", Held)];
        assert_eq!(waits_on(&c, "owner"), Waiter::Nobody);
        assert_eq!(waits_on(&[], "owner"), Waiter::Nobody);
    }

    #[test_id(tl6dg57)]
    #[test]
    fn a_held_comment_never_decides_the_waiter() {
        let c = [("ada", Now), ("bo", Held)];
        assert_eq!(waits_on(&c, "owner"), Waiter::Reader);
        let c = [("reader", Now), ("ada", Held)];
        assert_eq!(waits_on(&c, "owner"), Waiter::Agent("owner".into()));
    }
}
