use anyhow as ah;
use argh::FromArgs;
use async_shutdown::Shutdown;
use once_cell::sync::OnceCell;
use poise::{
    serenity_prelude::{self as serenity, ChannelId},
    FrameworkError,
};
use rusqlite as sql;
use sql::OptionalExtension;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

mod watcher;

#[derive(FromArgs)]
/// Reach new heights.
struct Args {
    /// path to config. Defaults to the exe_dir/quotebot.conf
    #[argh(option)]
    config_path: Option<String>,
}

struct Config {
    token: String,
    db_path: String,
    quotes_channel_id: u64,
    quotes_db_path: String,
}

fn get_config() -> &'static Config {
    static CONFIG: OnceCell<Config> = OnceCell::new();
    CONFIG.get_or_init(|| {
        let args: Args = argh::from_env();
        let config_path = args.config_path.unwrap_or("quotebot.conf".to_owned());
        let mut config = configparser::ini::Ini::new();
        config.load(config_path).expect("Couldn't read config file");

        Config {
            token: config
                .get("default", "token")
                .expect("Config: token must be specified."),
            quotes_channel_id: config
                .getuint("default", "quotes_channel_id")
                .expect("channel id must be u64")
                .expect("Config: quotes_channel_id required"),
            db_path: config
                .get("default", "db_file")
                .expect("Config: db_file must be specified"),
            quotes_db_path: config
                .get("default", "quotes_db_path")
                .expect("Config: quotes_db_path must be specified."),
        }
    })
}

fn get_db() -> ah::Result<sql::Connection> {
    let path = &get_config().db_path;
    let conn = sql::Connection::open(path)?;

    static DB_INIT: OnceCell<()> = OnceCell::new();
    DB_INIT.get_or_try_init(|| {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS credentials (
                   discord_id                INTEGER PRIMARY KEY,
                   auth_user                 TEXT,
                   auth_pass                 TEXT
                   )",
            [],
        )
        .and(Ok(()))
    })?;
    Ok(conn)
}

fn get_client() -> &'static reqwest::Client {
    static CLIENT: OnceCell<reqwest::Client> = OnceCell::new();
    CLIENT.get_or_init(|| reqwest::Client::new())
}

#[derive(Debug, Clone)]
struct Quote {
    id: i64,
    text: String,
    tags: Option<String>,
}

#[derive(Debug)]
struct Data {
    poll_tx: mpsc::Sender<()>,
}
type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

/// Register your titanic login so that you can send quotes from discord.
#[poise::command(slash_command, dm_only)]
async fn register(
    ctx: Context<'_>,
    #[description = "titanic user"] user: String,
    #[description = "titanic pass"] pass: String,
) -> Result<(), Error> {
    if !get_client()
        .get("https://blacker.caltech.edu/quotes/")
        .basic_auth(&user, Some(&pass))
        .send()
        .await?
        .status()
        .is_success()
    {
        Err(ah::anyhow!("Credentials didn't work"))?
    }

    let discord_user_id = ctx.author().id.as_u64();
    let conn = get_db()?;
    conn.execute(
        "INSERT OR REPLACE INTO credentials (discord_id, auth_user, auth_pass) VALUES (?1, ?2, ?3
  )",
        sql::params![discord_user_id, user, pass],
    )?;

    poise::say_reply(
        ctx,
        "Successfully updated your registration. You can now send quotes!",
    )
    .await?;

    Ok(())
}

fn quote_help() -> String {
    String::from(
        "\
Accessible via /quote or ~quote, in the server or in DMs. ~quote will show to
other people in the server. Usually, people don't see who submits hellquotes, so
consider using /quote or ~quote in DMs. ~quote is the only way to write multiple
lines. To add tags, prefix tag:[tag] as many times as you want, separated by
spaces.

Example usage:
~quote tag:anon tag:blacker my awesome quote

-anonymous",
    )
}

/// Send a quote. For multiple lines, use ~quote not /quote. For anonymity, use /quote or DMs.
#[poise::command(slash_command, prefix_command, help_text_fn = "quote_help")]
async fn quote(
    ctx: Context<'_>,
    #[description = "quote text, preceeded by zero or more space-separated \"tag:[tag]\"s"]
    #[rest]
    text: String,
) -> Result<(), Error> {
    let mut tag_string = String::new();
    let mut iter = text.split_whitespace().peekable();
    const PATTERN: &str = "tag:";
    while let Some(tag) = iter
        .peek()
        .ok_or(ah::anyhow!(
            "Message must have a non-empty, non-tag portion."
        ))?
        .strip_prefix(PATTERN)
    {
        tag_string.push_str(tag);
        tag_string.push(' ');
        iter.next();
    }
    tag_string.pop();

    let quote = {
        let quote_string_start_slice = iter.next().ok_or(ah::anyhow!(
            "Message must have a non-empty, non-tag portion."
        ))?;
        let quote_string_start_index =
            quote_string_start_slice.as_ptr() as usize - text.as_ptr() as usize;
        text[quote_string_start_index..].trim_end()
    };

    let discord_id = ctx.author().id.as_u64();

    let conn = get_db()?;
    let (user, pass): (String, String) = conn
        .query_row(
            "SELECT auth_user, auth_pass FROM credentials where discord_id = ?1",
            [discord_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or(ah::anyhow!(
            "You aren't registered, try DMing me the /register command"
        ))?;

    let response = get_client()
        .post("https://blacker.caltech.edu/quotes/")
        .basic_auth(user, Some(pass))
        .form(&[("quote", quote), ("tags", &tag_string)])
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(ah::anyhow!("Hellquotes gave status: {}", response.status()).into());
    }

    // if this is a slash cmd, send an invisible reply so that we don't get a
    // "no response" error message sent to the user
    if let Context::Application(actx) = ctx {
        poise::reply::send_application_reply(actx, |cr| cr.content("Success.").ephemeral(true))
            .await?;
    };

    // prompt quote watcher to check for the newly submitted quote so it shows
    // up faster
    ctx.data().poll_tx.try_send(()).ok();

    Ok(())
}

fn truncate_str(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some((idx, _)) => &s[..idx],
    }
}

async fn send_quote(quote: &Quote, http: &serenity::Http) -> ah::Result<()> {
    info!(id = quote.id, "Submitting quote to discord");
    // truncate quote text to ensure message is under 2000 chars
    let text = truncate_str(&quote.text, 1600);
    let tags = truncate_str(quote.tags.as_ref().map(String::as_str).unwrap_or(""), 200);

    ChannelId::from(get_config().quotes_channel_id)
        .send_message(http, |msg| {
            msg.embed(|embed| {
                embed
                    .title(text)
                    .description(format!(
                        "[View on Titanic](https://blacker.caltech.edu/quotes/?q={})",
                        quote.id
                    ))
                    .color(0)
                    .footer(|footer| footer.text(format!("Tags: {}", tags)))
            })
        })
        .await?;
    Ok(())
}

#[poise::command(prefix_command, slash_command)]
async fn help(
    ctx: Context<'_>,
    #[description = "Specific command to show help about"] command: Option<String>,
) -> Result<(), Error> {
    let config = poise::builtins::HelpConfiguration {
        extra_text_at_bottom: "Type /help command for more info on a command.",
        ..Default::default()
    };
    poise::builtins::help(ctx, command.as_deref(), config).await?;
    Ok(())
}

async fn on_error(e: FrameworkError<'_, Data, Error>) {
    use FrameworkError::*;
    match e {
        Setup { error, .. } => error!("Setup failed: {}", error),
        EventHandler { error, .. } => error!("Error during event handler: {}", error),
        Command { error, ctx } => {
            let user_error_msg = error.to_string();
            if let Err(e) = poise::say_reply(ctx, user_error_msg).await {
                error!("Error while user command error: {}", e);
            }
        }
        ArgumentParse { error, ctx, .. } => {
            let mut usage = "Please check the help menu for usage information".into();
            if let Some(help_text) = ctx.command().help_text {
                usage = help_text();
            }
            let user_error_msg = format!("**{}**\n{}", error, usage);
            if let Err(e) = poise::say_reply(ctx, user_error_msg).await {
                error!("Error while user command error: {}", e);
            }
        }
        GuildOnly { ctx } => {
            if let Err(e) = poise::say_reply(ctx, "Command is only allowed in servers!").await {
                error!("Error while user command error: {}", e);
            }
        }
        DmOnly { ctx } => {
            if let Err(e) = poise::say_reply(ctx, "Command is only allowed in DM").await {
                error!("Error while user command error: {}", e);
            }
        }
        UnknownCommand { .. } => error!("Somehow got an unknown command error?"),
        _ => error!("UNHANDLED ERROR OCCURRED: {:?}", e),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ah::Result<()> {
    tracing_subscriber::fmt::init();

    let shutdown = Shutdown::new();
    let shutdown_ = shutdown.clone();
    // Spawn a task to wait for CTRL+C and trigger a shutdown.
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            if let Err(e) = tokio::signal::ctrl_c().await {
                error!("Failed to wait for CTRL+C: {}", e);
                std::process::exit(1);
            } else {
                warn!("\nReceived interrupt signal. Shutting down server...");
                shutdown.shutdown();
            }
        }
    });

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![register(), quote(), help()],
            on_error: |e| Box::pin(on_error(e)),
            prefix_options: poise::PrefixFrameworkOptions {
                prefix: Some("~".into()),
                edit_tracker: None,
                case_insensitive_commands: true,
                ..Default::default()
            },
            ..Default::default()
        })
        .token(&get_config().token)
        .intents(
            serenity::GatewayIntents::MESSAGE_CONTENT
                | serenity::GatewayIntents::GUILD_MESSAGES
                | serenity::GatewayIntents::DIRECT_MESSAGES,
        )
        .setup(|ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                info!("Bot setup complete.");

                let quote_db_path = &get_config().quotes_db_path;

                let (poll_tx, poller_task) =
                    watcher::create_poller(ctx.http.clone(), quote_db_path, shutdown_.clone())?;
                let poller_task = shutdown_.wrap_vital(poller_task);
                let poller_task = shutdown_.wrap_cancel(poller_task);
                tokio::spawn(poller_task);

                let fs_watcher = watcher::configure_fs_watcher(poll_tx.clone(), quote_db_path)?;
                // de-allocate the watcher when we're done using a never-finishing task
                let watcher_task = async move {
                    let _fs_watcher = fs_watcher;
                    let () = std::future::pending().await;
                };
                tokio::spawn(shutdown_.wrap_cancel(watcher_task));

                Ok(Data { poll_tx })
            })
        });
    let bot_run = framework.run();
    let bot_run = shutdown.wrap_vital(shutdown.wrap_cancel(bot_run));
    match bot_run.await {
        None => return Ok(info!("Main bot loop cancelled by shutdown.")),
        Some(Err(e)) => return Err(e.into()),
        Some(Ok(_)) => unreachable!(), // bot loop never exits
    }
}
