//! The orders a forum's posts can be listed in, worked out on this device from the
//! posts, replies and votes it has loaded.

use super::Post;

#[derive(Clone, Copy, Default, PartialEq)]
pub(super) enum Order {
    /// Newest reply or post first.
    #[default]
    Active,
    /// Newest post first.
    New,
    /// Most up votes over down votes.
    Top,
    /// Votes, weighed down as the post gets older.
    Hot,
    /// No replies yet, newest first.
    Unanswered,
}

impl Order {
    pub(super) const ALL: [Self; 5] = [
        Self::Active,
        Self::New,
        Self::Top,
        Self::Hot,
        Self::Unanswered,
    ];

    pub(super) fn icon(self) -> &'static str {
        match self {
            Self::Active => "💬",
            Self::New => "🆕",
            Self::Top => "⭐",
            Self::Hot => "🔥",
            Self::Unanswered => "❓",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Active => t!("forum.sort_active"),
            Self::New => t!("forum.sort_new"),
            Self::Top => t!("forum.sort_top"),
            Self::Hot => t!("forum.sort_hot"),
            Self::Unanswered => t!("forum.sort_unanswered"),
        }
    }

    pub(super) fn hint(self) -> &'static str {
        match self {
            Self::Active => t!("forum.sort_active_help"),
            Self::New => t!("forum.sort_new_help"),
            Self::Top => t!("forum.sort_top_help"),
            Self::Hot => t!("forum.sort_hot_help"),
            Self::Unanswered => t!("forum.sort_unanswered_help"),
        }
    }

    /// Whether it counts only posts from a stretch of time.
    pub(super) fn timed(self) -> bool {
        self == Self::Top
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
pub(super) enum Period {
    Day,
    Month,
    Year,
    #[default]
    All,
}

impl Period {
    pub(super) const ALL: [Self; 4] = [Self::Day, Self::Month, Self::Year, Self::All];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Day => t!("forum.period_day"),
            Self::Month => t!("forum.period_month"),
            Self::Year => t!("forum.period_year"),
            Self::All => t!("forum.period_all"),
        }
    }

    fn seconds(self) -> Option<i64> {
        match self {
            Self::Day => Some(86_400),
            Self::Month => Some(30 * 86_400),
            Self::Year => Some(365 * 86_400),
            Self::All => None,
        }
    }
}

fn score(post: &Post) -> i64 {
    post.up as i64 - post.down as i64
}

/// A score that a newer post needs fewer votes to match, so the list keeps moving.
fn hot(post: &Post, now: i64) -> f64 {
    let hours = (now - post.msg.at).max(0) as f64 / 3600.0;
    (score(post) as f64 + 1.0) / (hours + 2.0).powf(1.5)
}

/// Sorts `posts` (unix seconds `now`), leaving out what the order doesn't count.
pub(super) fn sort(posts: &mut Vec<Post>, order: Order, period: Period, now: i64) {
    if let Some(span) = period.seconds().filter(|_| order.timed()) {
        posts.retain(|post| now - post.msg.at <= span);
    }
    if order == Order::Unanswered {
        posts.retain(|post| post.replies == 0);
    }
    let newest = |a: &Post, b: &Post| b.msg.at.cmp(&a.msg.at);
    match order {
        Order::Active => posts.sort_by_key(|post| std::cmp::Reverse(post.last)),
        Order::New | Order::Unanswered => posts.sort_by(newest),
        Order::Top => posts.sort_by(|a, b| score(b).cmp(&score(a)).then(newest(a, b))),
        Order::Hot => posts.sort_by(|a, b| hot(b, now).total_cmp(&hot(a, now))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{Body, Msg};

    fn post(id: u8, at: i64, up: usize, down: usize, replies: usize) -> Post {
        Post {
            msg: Msg {
                backfilled: false,
                id: [id; 32],
                quoted_author: None,
                person: [1; 32],
                at,
                reply_to: None,
                edit_of: None,
                body: Body::Text(String::new()),
                files: Vec::new(),
                live: None,
                sticker: None,
                replicas: Vec::new(),
                copy_files: Vec::new(),
            },
            poll: None,
            replies,
            last: at + replies as i64,
            up,
            down,
        }
    }

    fn sorted(order: Order, period: Period) -> Vec<u8> {
        let day = 86_400;
        let mut posts = vec![
            post(1, 100 * day, 9, 0, 3),
            post(2, 400 * day - 3600, 2, 0, 0),
            post(3, 390 * day, 5, 5, 1),
            post(4, 399 * day, 0, 1, 0),
        ];
        sort(&mut posts, order, period, 400 * day);
        posts.iter().map(|post| post.msg.id[0]).collect()
    }

    #[test]
    fn each_order_puts_its_kind_of_post_first() {
        assert_eq!(sorted(Order::New, Period::All), [2, 4, 3, 1]);
        assert_eq!(sorted(Order::Top, Period::All), [1, 2, 3, 4]);
        // Only the last month counts: the old favourite drops out.
        assert_eq!(sorted(Order::Top, Period::Month), [2, 3, 4]);
        assert_eq!(sorted(Order::Top, Period::Day), [2, 4]);
        assert_eq!(sorted(Order::Unanswered, Period::All), [2, 4]);
        // Fresh with a few votes beats old with many.
        assert_eq!(sorted(Order::Hot, Period::All)[0], 2);
    }
}
