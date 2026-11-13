//! Running SQL with a fetch limit, cancellation and a timeout.
//!
//! A run reads rows lazily: it stops at the fetch limit and keeps the server's result open until
//! the caller asks for the next page, everything, or cancels. While it is open the session is
//! busy, the same way a JDBC result set ties up its connection.

use std::{
    future::Future,
    pin::pin,
    time::{Duration, Instant},
};

use anyhow::Result;
use futures::{Stream, StreamExt as _, channel::mpsc};

use crate::{ColumnInfo, ResultSet, StatementOutcome};

/// Rows read from each result set before pausing, unless the connection sets its own limit.
pub const DEFAULT_FETCH_LIMIT: usize = 500;
/// Rows are sent in batches so the grid updates while a large page streams in.
const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const MAX_BATCH_ROWS: usize = 1000;
/// When settling an interrupted result, how long to wait for the item being read to finish.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fetch {
    NextPage,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    Fetch(Fetch),
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionEvent {
    ResultSet(Vec<ColumnInfo>),
    /// More rows for the latest result set. `None` is SQL `NULL`.
    Rows(Vec<Vec<Option<String>>>),
    RowsAffected(u64),
    /// The latest result set has more rows. Reading resumes when the handle asks for them.
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionEnd {
    Completed,
    /// Stopped on request. Rows already sent are kept.
    Cancelled,
    TimedOut(Duration),
}

/// Why a run stopped before the server finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Interrupt {
    Cancelled,
    TimedOut(Duration),
}

impl From<Interrupt> for ExecutionEnd {
    fn from(interrupt: Interrupt) -> Self {
        match interrupt {
            Interrupt::Cancelled => ExecutionEnd::Cancelled,
            Interrupt::TimedOut(timeout) => ExecutionEnd::TimedOut(timeout),
        }
    }
}

/// A run was stopped but the session couldn't be brought back in sync with the server, so it
/// was closed. The pool reconnects on the next use.
#[derive(Debug)]
pub struct SessionReset(pub String);

impl std::fmt::Display for SessionReset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the statement was stopped, but the session had to be closed ({}); temporary tables \
             and open transactions in it are gone",
            self.0
        )
    }
}

impl std::error::Error for SessionReset {}

/// What a driver produces while reading a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    Columns(Vec<ColumnInfo>),
    Row(Vec<Option<String>>),
    RowsAffected(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadEnd {
    Exhausted,
    Interrupted(Interrupt),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ExecutionOptions {
    /// Rows read from each result set before pausing. `None` reads everything.
    pub fetch_limit: Option<usize>,
    /// How long the server may work before the statement is cancelled. Time spent paused at the
    /// fetch limit doesn't count. `None` waits indefinitely.
    pub timeout: Option<Duration>,
}

/// Controls a run from the UI: ask for more rows or stop it. Dropping every handle cancels it.
#[derive(Clone, Debug)]
pub struct ExecutionHandle {
    requests: mpsc::UnboundedSender<Request>,
}

impl ExecutionHandle {
    pub fn fetch(&self, fetch: Fetch) {
        self.send(Request::Fetch(fetch));
    }

    pub fn cancel(&self) {
        self.send(Request::Cancel);
    }

    fn send(&self, request: Request) {
        if self.requests.unbounded_send(request).is_err() {
            log::debug!("ignored {request:?}: the run already finished");
        }
    }
}

/// The driver's side of a run: requests from the handle and events for the UI.
pub struct Execution {
    options: ExecutionOptions,
    /// `None` when nothing can interrupt the run.
    requests: Option<mpsc::UnboundedReceiver<Request>>,
    events: mpsc::UnboundedSender<ExecutionEvent>,
    /// When the current stretch of server work times out. Reset when a paused run resumes.
    deadline: Option<tokio::time::Instant>,
}

/// Creates a run that pauses at the fetch limit and can be cancelled through the handle.
pub fn execution(
    options: ExecutionOptions,
) -> (
    ExecutionHandle,
    Execution,
    mpsc::UnboundedReceiver<ExecutionEvent>,
) {
    let (request_sender, request_receiver) = mpsc::unbounded();
    let (event_sender, event_receiver) = mpsc::unbounded();
    let handle = ExecutionHandle {
        requests: request_sender,
    };
    let execution = Execution {
        options: ExecutionOptions {
            fetch_limit: options.fetch_limit.filter(|limit| *limit > 0),
            timeout: options.timeout.filter(|timeout| !timeout.is_zero()),
        },
        requests: Some(request_receiver),
        events: event_sender,
        deadline: None,
    };
    (handle, execution.with_fresh_deadline(), event_receiver)
}

/// A run that reads everything and can't be interrupted, for internal statements and tests.
pub(crate) fn uninterruptible() -> (Execution, mpsc::UnboundedReceiver<ExecutionEvent>) {
    let (event_sender, event_receiver) = mpsc::unbounded();
    let execution = Execution {
        options: ExecutionOptions::default(),
        requests: None,
        events: event_sender,
        deadline: None,
    };
    (execution, event_receiver)
}

enum Step {
    Request(Option<Request>),
    TimedOut,
    Flush,
    Item(Option<Result<Item>>),
}

enum Resume {
    NextPage,
    All,
    Cancel,
}

impl Execution {
    fn with_fresh_deadline(mut self) -> Self {
        self.reset_deadline();
        self
    }

    fn reset_deadline(&mut self) {
        self.deadline = self
            .options
            .timeout
            .map(|timeout| tokio::time::Instant::now() + timeout);
    }

    fn timeout(&self) -> Duration {
        self.options.timeout.unwrap_or_default()
    }

    /// Starts the timeout when the statement goes to the server, so connecting doesn't count.
    pub(crate) fn start(&mut self) {
        self.reset_deadline();
    }

    pub(crate) fn emit(&self, event: ExecutionEvent) {
        if self.events.unbounded_send(event).is_err() {
            log::debug!("dropped a query event: nothing is listening");
        }
    }

    /// Whether the run was cancelled or timed out while nothing was in flight, such as between
    /// two batches of a script. Requests for more rows don't apply there and are dropped.
    pub(crate) fn interrupted(&mut self) -> Option<Interrupt> {
        if let Some(requests) = self.requests.as_mut() {
            loop {
                match requests.try_recv() {
                    Ok(Request::Fetch(_)) => {}
                    Ok(Request::Cancel) => return Some(Interrupt::Cancelled),
                    Err(error) if error.is_closed() => return Some(Interrupt::Cancelled),
                    Err(_) => break,
                }
            }
        }
        self.deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
            .then(|| Interrupt::TimedOut(self.timeout()))
    }

    /// Awaits `future`, unless the run is cancelled or times out first. The future is dropped
    /// when interrupted.
    pub(crate) async fn guard<F: Future>(&mut self, future: F) -> Result<F::Output, Interrupt> {
        let mut future = pin!(future);
        loop {
            let deadline = self.deadline;
            tokio::select! {
                biased;
                request = next_request(&mut self.requests) => match request {
                    Some(Request::Fetch(_)) => {}
                    Some(Request::Cancel) | None => return Err(Interrupt::Cancelled),
                },
                _ = sleep_until(deadline) => return Err(Interrupt::TimedOut(self.timeout())),
                output = &mut future => return Ok(output),
            }
        }
    }

    /// Turns `items` into events, pausing before the first row past the fetch limit of each
    /// result set. Stops early when cancelled or timed out, leaving the rest of `items` unread.
    pub(crate) async fn read<S>(&mut self, items: &mut S) -> Result<ReadEnd>
    where
        S: Stream<Item = Result<Item>> + Unpin,
    {
        let mut batch = Vec::new();
        let mut batch_started = Instant::now();
        let mut remaining = self.options.fetch_limit;
        loop {
            let deadline = self.deadline;
            let flush_at = (!batch.is_empty())
                .then(|| tokio::time::Instant::from_std(batch_started) + FLUSH_INTERVAL);
            let step = tokio::select! {
                biased;
                request = next_request(&mut self.requests) => Step::Request(request),
                _ = sleep_until(deadline) => Step::TimedOut,
                _ = sleep_until(flush_at) => Step::Flush,
                item = items.next() => Step::Item(item),
            };
            match step {
                Step::Request(Some(Request::Fetch(Fetch::All))) => remaining = None,
                Step::Request(Some(Request::Fetch(Fetch::NextPage))) => {}
                Step::Request(Some(Request::Cancel) | None) => {
                    self.flush(&mut batch);
                    return Ok(ReadEnd::Interrupted(Interrupt::Cancelled));
                }
                Step::TimedOut => {
                    self.flush(&mut batch);
                    return Ok(ReadEnd::Interrupted(Interrupt::TimedOut(self.timeout())));
                }
                Step::Flush => self.flush(&mut batch),
                Step::Item(None) => {
                    self.flush(&mut batch);
                    return Ok(ReadEnd::Exhausted);
                }
                Step::Item(Some(Err(error))) => {
                    self.flush(&mut batch);
                    return Err(error);
                }
                Step::Item(Some(Ok(Item::Columns(columns)))) => {
                    self.flush(&mut batch);
                    self.emit(ExecutionEvent::ResultSet(columns));
                    remaining = self.options.fetch_limit;
                }
                Step::Item(Some(Ok(Item::RowsAffected(count)))) => {
                    self.flush(&mut batch);
                    self.emit(ExecutionEvent::RowsAffected(count));
                }
                Step::Item(Some(Ok(Item::Row(row)))) => {
                    // Pausing only once a further row exists means "more rows" is never a guess.
                    if remaining == Some(0) {
                        self.flush(&mut batch);
                        self.emit(ExecutionEvent::Paused);
                        match self.wait_while_paused().await {
                            Resume::NextPage => remaining = self.options.fetch_limit,
                            Resume::All => remaining = None,
                            Resume::Cancel => {
                                return Ok(ReadEnd::Interrupted(Interrupt::Cancelled));
                            }
                        }
                        self.reset_deadline();
                    }
                    if batch.is_empty() {
                        batch_started = Instant::now();
                    }
                    batch.push(row);
                    if let Some(remaining) = remaining.as_mut() {
                        *remaining = remaining.saturating_sub(1);
                    }
                    if batch.len() >= MAX_BATCH_ROWS {
                        self.flush(&mut batch);
                    }
                }
            }
        }
    }

    async fn wait_while_paused(&mut self) -> Resume {
        let Some(requests) = self.requests.as_mut() else {
            return Resume::All;
        };
        match requests.next().await {
            Some(Request::Fetch(Fetch::NextPage)) => Resume::NextPage,
            Some(Request::Fetch(Fetch::All)) => Resume::All,
            Some(Request::Cancel) | None => Resume::Cancel,
        }
    }

    fn flush(&self, batch: &mut Vec<Vec<Option<String>>>) {
        if !batch.is_empty() {
            self.emit(ExecutionEvent::Rows(std::mem::take(batch)));
        }
    }
}

async fn next_request(requests: &mut Option<mpsc::UnboundedReceiver<Request>>) -> Option<Request> {
    match requests {
        Some(requests) => requests.next().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    /// The rest of the result was read; nothing is left in flight.
    Ended,
    /// The result is still open, and reading stopped right after an item, or while the server
    /// was silent, which is between items unless it stalled mid-row.
    Open,
}

/// Reads on and discards what's left of an interrupted result for up to `budget`, then lets an
/// item that is partly read finish. A small remainder is often already on the wire, and
/// stopping between items lets a driver cancel without losing its place in the protocol.
pub(crate) async fn settle<S>(
    items: &mut S,
    budget: Duration,
    is_fatal: impl Fn(&anyhow::Error) -> bool,
) -> Result<Settled>
where
    S: Stream<Item = Result<Item>> + Unpin,
{
    let deadline = tokio::time::Instant::now() + budget;
    // A timeout only fires while its future waits, and a fast stream never does: check the
    // clock between items too.
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, items.next()).await {
            Ok(None) => return Ok(Settled::Ended),
            Ok(Some(Err(error))) if is_fatal(&error) => return Err(error),
            Ok(Some(_)) => {}
            Err(_) => break,
        }
    }
    // `next` keeps its progress in the stream, so this resumes the item it was reading.
    match tokio::time::timeout(SETTLE_TIMEOUT, items.next()).await {
        Ok(None) => Ok(Settled::Ended),
        Ok(Some(Err(error))) if is_fatal(&error) => Err(error),
        Ok(Some(_)) | Err(_) => Ok(Settled::Open),
    }
}

/// Folds a finished run's events into one outcome per statement.
pub fn collect_outcomes(events: impl IntoIterator<Item = ExecutionEvent>) -> Vec<StatementOutcome> {
    let mut outcomes = Vec::new();
    let mut current: Option<ResultSet> = None;
    for event in events {
        match event {
            ExecutionEvent::ResultSet(columns) => {
                if let Some(result) = current.take() {
                    outcomes.push(StatementOutcome::Rows(result));
                }
                current = Some(ResultSet {
                    columns,
                    rows: Vec::new(),
                });
            }
            ExecutionEvent::Rows(rows) => {
                if let Some(result) = current.as_mut() {
                    result.rows.extend(rows);
                }
            }
            ExecutionEvent::RowsAffected(count) => {
                if let Some(result) = current.take() {
                    outcomes.push(StatementOutcome::Rows(result));
                }
                outcomes.push(StatementOutcome::RowsAffected(count));
            }
            ExecutionEvent::Paused => {}
        }
    }
    if let Some(result) = current.take() {
        outcomes.push(StatementOutcome::Rows(result));
    }
    if outcomes.is_empty() {
        outcomes.push(StatementOutcome::Completed);
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn columns(names: &[&str]) -> Item {
        Item::Columns(
            names
                .iter()
                .map(|name| ColumnInfo {
                    name: name.to_string(),
                    type_name: String::new(),
                })
                .collect(),
        )
    }

    fn row(value: usize) -> Result<Item> {
        Ok(Item::Row(vec![Some(value.to_string())]))
    }

    fn rows_in(events: &[ExecutionEvent]) -> usize {
        events
            .iter()
            .map(|event| match event {
                ExecutionEvent::Rows(rows) => rows.len(),
                _ => 0,
            })
            .sum()
    }

    fn received(events: &mut mpsc::UnboundedReceiver<ExecutionEvent>) -> Vec<ExecutionEvent> {
        let mut received = Vec::new();
        while let Ok(event) = events.try_recv() {
            received.push(event);
        }
        received
    }

    fn table(count: usize) -> impl Stream<Item = Result<Item>> + Unpin + Send + 'static {
        stream::iter(std::iter::once(Ok(columns(&["n"]))).chain((0..count).map(row)))
    }

    fn spawn_read(
        mut execution: Execution,
        mut items: impl Stream<Item = Result<Item>> + Unpin + Send + 'static,
    ) -> tokio::task::JoinHandle<Result<ReadEnd>> {
        tokio::spawn(async move { execution.read(&mut items).await })
    }

    async fn wait_for_pause(
        events: &mut mpsc::UnboundedReceiver<ExecutionEvent>,
    ) -> Vec<ExecutionEvent> {
        let mut received = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.next())
                .await
                .expect("timed out waiting for the run to pause")
                .expect("the run ended without pausing");
            let paused = event == ExecutionEvent::Paused;
            received.push(event);
            if paused {
                return received;
            }
        }
    }

    fn limited(fetch_limit: usize) -> ExecutionOptions {
        ExecutionOptions {
            fetch_limit: Some(fetch_limit),
            timeout: None,
        }
    }

    #[tokio::test]
    async fn pauses_at_the_fetch_limit_and_resumes_by_page() -> Result<()> {
        let (handle, run, mut events) = execution(limited(10));
        let reader = spawn_read(run, table(25));

        let first = wait_for_pause(&mut events).await;
        assert!(matches!(first[0], ExecutionEvent::ResultSet(_)));
        assert_eq!(rows_in(&first), 10);

        handle.fetch(Fetch::NextPage);
        assert_eq!(rows_in(&wait_for_pause(&mut events).await), 10);

        handle.fetch(Fetch::NextPage);
        assert_eq!(reader.await??, ReadEnd::Exhausted);
        let last = received(&mut events);
        assert_eq!(rows_in(&last), 5);
        assert!(!last.contains(&ExecutionEvent::Paused));
        Ok(())
    }

    #[tokio::test]
    async fn a_result_of_exactly_one_page_does_not_pause() -> Result<()> {
        let (_handle, mut run, mut events) = execution(limited(10));
        let mut items = table(10);
        assert_eq!(run.read(&mut items).await?, ReadEnd::Exhausted);
        let events = received(&mut events);
        assert_eq!(rows_in(&events), 10);
        assert!(!events.contains(&ExecutionEvent::Paused));
        Ok(())
    }

    #[tokio::test]
    async fn fetch_all_reads_to_the_end() -> Result<()> {
        let (handle, run, mut events) = execution(limited(10));
        let reader = spawn_read(run, table(5000));
        wait_for_pause(&mut events).await;
        handle.fetch(Fetch::All);
        assert_eq!(reader.await??, ReadEnd::Exhausted);
        assert_eq!(rows_in(&received(&mut events)), 4990);
        Ok(())
    }

    #[tokio::test]
    async fn each_result_set_gets_its_own_first_page() -> Result<()> {
        let (handle, run, mut events) = execution(limited(3));
        let items = stream::iter(
            std::iter::once(Ok(columns(&["a"])))
                .chain((0..5).map(row))
                .chain([Ok(Item::RowsAffected(7)), Ok(columns(&["b"]))])
                .chain((0..2).map(row)),
        );
        let reader = spawn_read(run, items);
        let mut all_events = wait_for_pause(&mut events).await;
        assert_eq!(rows_in(&all_events), 3);
        handle.fetch(Fetch::NextPage);
        assert_eq!(reader.await??, ReadEnd::Exhausted);
        all_events.extend(received(&mut events));
        let outcomes = collect_outcomes(all_events);
        let [
            StatementOutcome::Rows(first),
            StatementOutcome::RowsAffected(7),
            StatementOutcome::Rows(second),
        ] = outcomes.as_slice()
        else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert_eq!(first.rows.len(), 5);
        assert_eq!(second.rows.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn cancel_stops_a_statement_waiting_on_the_server() -> Result<()> {
        let (handle, run, mut events) = execution(limited(100));
        // A statement that sent its first rows and then keeps the server busy.
        let items = stream::iter([Ok(columns(&["n"])), row(1), row(2)])
            .chain(stream::pending::<Result<Item>>());
        let reader = spawn_read(run, items);
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.cancel();
        assert_eq!(reader.await??, ReadEnd::Interrupted(Interrupt::Cancelled));
        // Rows read before the cancel are still delivered.
        assert_eq!(rows_in(&received(&mut events)), 2);
        Ok(())
    }

    #[tokio::test]
    async fn cancel_while_paused_and_dropping_the_handle_both_stop() -> Result<()> {
        let (handle, run, mut events) = execution(limited(5));
        let reader = spawn_read(run, table(50));
        wait_for_pause(&mut events).await;
        handle.cancel();
        assert_eq!(reader.await??, ReadEnd::Interrupted(Interrupt::Cancelled));

        let (handle, run, mut events) = execution(limited(5));
        let reader = spawn_read(run, table(50));
        wait_for_pause(&mut events).await;
        drop(handle);
        assert_eq!(reader.await??, ReadEnd::Interrupted(Interrupt::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn guard_cancels_before_the_server_answers() -> Result<()> {
        let (handle, mut run, _events) = execution(ExecutionOptions::default());
        let waiting = tokio::spawn(async move { run.guard(std::future::pending::<()>()).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.cancel();
        assert_eq!(waiting.await?, Err(Interrupt::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn timeout_interrupts_server_work_but_not_a_paused_result() -> Result<()> {
        let timeout = Duration::from_millis(50);
        let (_handle, mut run, _events) = execution(ExecutionOptions {
            fetch_limit: None,
            timeout: Some(timeout),
        });
        let result = run.guard(std::future::pending::<()>()).await;
        assert_eq!(result, Err(Interrupt::TimedOut(timeout)));

        let (handle, run, mut events) = execution(ExecutionOptions {
            fetch_limit: Some(2),
            timeout: Some(timeout),
        });
        let reader = spawn_read(run, table(3).chain(stream::pending::<Result<Item>>()));
        wait_for_pause(&mut events).await;
        // Paused for longer than the timeout: still paused, not timed out.
        tokio::time::sleep(timeout * 3).await;
        assert!(!reader.is_finished());
        handle.fetch(Fetch::NextPage);
        assert_eq!(
            reader.await??,
            ReadEnd::Interrupted(Interrupt::TimedOut(timeout))
        );
        Ok(())
    }

    #[tokio::test]
    async fn uninterruptible_runs_collect_every_row() -> Result<()> {
        let (mut run, mut events) = uninterruptible();
        let mut items = table(1234);
        assert_eq!(run.read(&mut items).await?, ReadEnd::Exhausted);
        drop(run);
        let outcomes = collect_outcomes(received(&mut events));
        let [StatementOutcome::Rows(result)] = outcomes.as_slice() else {
            panic!("expected one result set: {outcomes:?}");
        };
        assert_eq!(result.rows.len(), 1234);
        Ok(())
    }
}
