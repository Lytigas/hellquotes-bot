use anyhow as ah;
use async_shutdown::Shutdown;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use poise::serenity_prelude::Http;
use rusqlite as sql;
use std::ops::Deref;
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::{send_quote, Quote};

pub struct QuoteWatcher {
    db_conn: sql::Connection,
}

// TODO: this could/should use the timestamp of the created quote with and an
// index on that field to avoid a full table scan. That's a little unreliable
// in sqlite because there's no actual date type, however. For now, this is
// slow, but general, and will work with any changes to the quote db schema.
impl QuoteWatcher {
    fn new(db_path: &str) -> ah::Result<Self> {
        let db_conn = sql::Connection::open_in_memory()?;
        // attach quotes db as read-only
        let ro_uri = format!("file:{}?mode=ro", db_path);
        db_conn.execute("ATTACH DATABASE ?1 as quotes", [ro_uri])?;
        db_conn.execute("CREATE TABLE main.seen_quotes (id INTEGER PRIMARY KEY)", [])?;
        // initialize with existing quotes
        Self::update_seen(&db_conn)?;
        Ok(Self { db_conn })
    }

    fn update_seen(db_conn: &sql::Connection) -> sql::Result<usize> {
        db_conn.execute(
            "
        INSERT OR REPLACE INTO main.seen_quotes SELECT id from quotes.quotes",
            [],
        )
    }

    fn get_new_and_update_seen(&mut self) -> ah::Result<impl Iterator<Item = Quote>> {
        let tx = self.db_conn.transaction()?;
        let new = {
            let mut stmt = tx.prepare(
            "SELECT id, quote, tags FROM quotes.quotes WHERE id NOT IN (SELECT id FROM main.seen_quotes)")?;
            let results = stmt
                .query_map([], |r| {
                    Ok(Quote {
                        id: r.get(0)?,
                        text: r.get(1)?,
                        tags: r.get(2)?,
                    })
                })?
                .collect::<Result<Vec<Quote>, _>>()?;
            results
        };
        Self::update_seen(tx.deref())?;
        tx.commit()?;
        Ok(new.into_iter())
    }
}

#[allow(dead_code)]
pub async fn send_timed_checks(sender: mpsc::Sender<()>, shutdown: Shutdown) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(2500));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match sender.try_send(()) {
            Err(TrySendError::Closed(())) => {
                eprintln!("Watcher task receiver droppped. Assuming fatal watcher crash.");
                shutdown.shutdown();
            }
            _ => (),
        }
    }
}

pub fn configure_fs_watcher(
    sender: mpsc::Sender<()>,
    db_path: &str,
) -> ah::Result<RecommendedWatcher> {
    let mut watcher = RecommendedWatcher::new(
        // No error handling: a full queue means a flush is already pending,
        // a dropped queue means the app is shutting down.
        move |_res| {
            sender.try_send(()).ok();
        },
        notify::Config::default(),
    )?;
    watcher.watch(db_path.as_ref(), RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

pub fn create_poller(
    disc_http: std::sync::Arc<Http>,
    db_path: &str,
    shutdown: Shutdown,
) -> ah::Result<(
    mpsc::Sender<()>,
    impl std::future::Future<Output = ()> + Send,
)> {
    // Since any check catches all changes, if there's an outstanding change when a poll
    // request is sent, the originating request is guaranteed to be handled anyway.
    // Thus, we don't need more than 2 slots, 3 to be safe.
    let (notify_tx, mut notify_rx) = mpsc::channel(3);

    // the task is going to run on a separate thread to avoid !Sync issues.
    let (quote_tx, mut quote_rx) = mpsc::unbounded_channel();
    let poller_token = shutdown.vital_token();
    let db_path = db_path.to_owned();
    std::thread::Builder::new()
        .name("db_watcher".to_string())
        .spawn(move || {
            let _shutdown_guard = poller_token;
            let mut watcher = QuoteWatcher::new(&db_path).expect("Couldn't create watcher");
            while let Some(()) = notify_rx.blocking_recv() {
                for quote in watcher
                    .get_new_and_update_seen()
                    .expect("Couldn't poll quotes")
                {
                    quote_tx.send(quote).expect("Couldn't send quote");
                }
            }
        })?;

    let poll_task = async move {
        while let Some(quote) = quote_rx.recv().await {
            send_quote(&quote, &disc_http)
                .await
                .expect("Couldn't send quote");
        }
    };
    let poll_task = shutdown.wrap_vital(poll_task);

    Ok((notify_tx, poll_task))
}
