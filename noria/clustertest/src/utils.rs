use mysql_async::prelude::{FromRow, Queryable, StatementLike};
use mysql_async::{Conn, Params};
use std::time::{Duration, Instant};
use tokio::time::sleep;

fn equal_rows<T>(a: &[T], b: &[T]) -> bool
where
    T: FromRow + std::cmp::PartialEq,
{
    let matching = a.iter().zip(b.iter()).filter(|&(a, b)| a == b).count();
    matching == a.len() && matching == b.len()
}

pub struct UntilResults<'a, T> {
    intermediate: &'a [&'a [T]],
    expected: &'a [T],
}

impl<'a, T> UntilResults<'a, T> {
    /// The results to a query should either be empty or the expected
    /// value. All other values should be rejected.
    pub fn empty_or(expected: &'a [T]) -> Self {
        Self {
            intermediate: &[&[]],
            expected,
        }
    }
}

/// Returns true when a prepare and execute returns the expected results,
/// [`UntilResults::expected`]. If `timeout` is reached, or the query returns a
/// value that is not in [`UntilResults::intermediate`], return false.
///
/// This function should be used in place of sleeping and executing a query after
/// the write propagation delay. It can also be used to assert that ReadySet
/// returns eventually consistent results, while waiting for an expected result.
pub async fn query_until_expected<S, T, P>(
    conn: &mut Conn,
    query: S,
    params: P,
    results: UntilResults<'_, T>,
    timeout: Duration,
) -> bool
where
    S: StatementLike + Clone,
    P: Into<Params> + Clone + std::marker::Send,
    T: FromRow + std::cmp::PartialEq + std::marker::Send + std::fmt::Debug + Clone + 'static,
{
    let mut last: Option<Vec<T>> = None;
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            println!("query_until_expected timed out, last: {:?}", last);
            return false;
        }
        let remaining = std::cmp::min(Duration::from_secs(5), timeout - start.elapsed());
        let result =
            tokio::time::timeout(remaining, conn.exec(query.clone(), params.clone())).await;
        match result {
            Ok(Ok(r)) => {
                if equal_rows(&r, results.expected) {
                    return true;
                }
                if !results
                    .intermediate
                    .iter()
                    .any(|intermediate| equal_rows(&r, intermediate))
                {
                    println!("Query results did not match accepted intermediate results. Results: {:?}, Accepted: {:?}", r, results.intermediate);
                    return false;
                }
                last = Some(r.clone());
            }
            Err(_) => {
                println!("Timed out when querying conn.");
            }
            _ => {}
        }

        sleep(Duration::from_millis(10)).await;
    }
}
