//! The limit of open file descriptors of the process: [`raise_fd_limit`].

use std::io;

use rustix::process::{Resource, getrlimit, setrlimit};

/// Raise the soft limit of the file descriptors that the process may have open to the hard limit,
/// and give the soft limit that it has then, `None` for none.
///
/// The next instance of a service takes the file descriptors of a hand-over out of the store under
/// its soft limit, and the kernel closes those that don't fit, so it raises its limit first, before
/// [`Store::take_over`](crate::Store::take_over). [`Handover::store`](crate::Handover::store)
/// fits the items of a hand-over under the soft limit that this instance had as its store was
/// created, which it takes the next one to have too: raise it before that.
///
/// The soft limit that systemd sets, 1024, holds a service to a few hundred clients, while the hard
/// one, 524288 under systemd, holds plenty. A process that waits on its file descriptors with
/// `select(2)` can't wait on those from 1024 on, so the limit is for those that wait with epoll or
/// `poll(2)`, as the async runtimes of Rust do.
pub fn raise_fd_limit() -> io::Result<Option<u64>> {
    let mut limit = getrlimit(Resource::Nofile);
    // `None` is no limit.
    let Some(current) = limit.current else {
        return Ok(None);
    };
    if limit.maximum.is_some_and(|maximum| maximum <= current) {
        return Ok(Some(current));
    }
    limit.current = limit.maximum;
    setrlimit(Resource::Nofile, limit)?;

    Ok(limit.current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::in_own_process;

    #[test]
    fn raised_to_the_hard_limit() {
        if !in_own_process(
            "limit::tests::raised_to_the_hard_limit",
            "ZOTER_TEST_LIMIT=1",
        ) {
            return;
        }
        let mut limit = getrlimit(Resource::Nofile);
        let Some(maximum) = limit.maximum else {
            // Nothing limits the file descriptors.
            assert_eq!(raise_fd_limit().unwrap(), limit.current);

            return;
        };
        limit.current = Some(maximum.min(64));
        setrlimit(Resource::Nofile, limit).unwrap();

        assert_eq!(raise_fd_limit().unwrap(), Some(maximum));
        assert_eq!(getrlimit(Resource::Nofile).current, Some(maximum));
        // There is nothing more to raise.
        assert_eq!(raise_fd_limit().unwrap(), Some(maximum));
    }
}
