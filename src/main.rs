use anyhow as ah;
use argh::FromArgs;
use once_cell::sync::OnceCell;
use poise::{
    serenity_prelude::{self as serenity, CacheHttp},
    FrameworkError,
};
use reqwest::header::HeaderValue;
use rusqlite as sql;
use sql::OptionalExtension;

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
            db_path: config
                .get("default", "db_file")
                .expect("Config: db_file must be specified"),
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

#[derive(Debug)]
struct Data();
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

/// Returns the URL if the header is canonical, otherwise None
fn get_url_from_header(h: &HeaderValue) -> Option<&str> {
    let s = h.to_str().ok()?;
    // Doesn't actually parse rel, but that's probably fine
    if !s.contains("canonical") {
        return None;
    }
    let a = s.find('<')?;
    let b = s.find('>')?;
    // Also doesn't handle quoting, shouldnt matter since we only display the url
    s.get(a + 1..b)
}

/// Send a quote. For multiline usage, use ~quote instead of /quote.
#[poise::command(slash_command, prefix_command, guild_only)]
async fn quote(
    ctx: Context<'_>,
    #[description = "quote text, preceeded by zero or more space-separated \"tag:[tag]\"s"]
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
        // .post("https://blacker.caltech.edu/quotes/")
        .post("https://webhook.site/a34dd23a-3cd4-4134-924d-6f4712f277d2")
        .basic_auth(user, Some(pass))
        .form(&[("quote", quote), ("tags", &tag_string)])
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(ah::anyhow!("Hellquotes gave status: {}", response.status()).into());
    }
    let url = response
        .headers()
        .get_all(reqwest::header::LINK)
        .iter()
        .filter_map(get_url_from_header)
        .next()
        .unwrap_or("No valid link found in response");

    // if this is a quote, send an invisible reply so that we don't get a
    // "no response" error message sent to the user
    if let Context::Application(actx) = ctx {
        poise::reply::send_application_reply(actx, |cr| cr.content("Success.").ephemeral(true))
            .await?;
    };

    ctx.channel_id()
        .send_message(ctx.http(), |msg| {
            msg.content(format!(
                "Quote submitted. Text:\n{}\n\nTags: {}\nView on titanic: {}",
                quote, tag_string, url
            ))
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
        extra_text_at_bottom: "Type ?help command for more info on a command.",
        ..Default::default()
    };
    poise::builtins::help(ctx, command.as_deref(), config).await?;
    Ok(())
}

async fn on_error(e: FrameworkError<'_, Data, Error>) {
    use FrameworkError::*;
    match e {
        Setup { error, .. } => eprintln!("Setup failed: {}", error),
        EventHandler { error, .. } => eprintln!("Error during event handler: {}", error),
        Command { error, ctx } => {
            let user_error_msg = error.to_string();
            if let Err(e) = poise::say_reply(ctx, user_error_msg).await {
                eprintln!("Error while user command error: {}", e);
            }
        }
        ArgumentParse { error, ctx, .. } => {
            let mut usage = "Please check the help menu for usage information".into();
            if let Some(help_text) = ctx.command().help_text {
                usage = help_text();
            }
            let user_error_msg = format!("**{}**\n{}", error, usage);
            if let Err(e) = poise::say_reply(ctx, user_error_msg).await {
                eprintln!("Error while user command error: {}", e);
            }
        }
        GuildOnly { ctx } => {
            if let Err(e) = poise::say_reply(ctx, "Command is only allowed in servers!").await {
                eprintln!("Error while user command error: {}", e);
            }
        }
        DmOnly { ctx } => {
            if let Err(e) = poise::say_reply(ctx, "Command is only allowed in DM").await {
                eprintln!("Error while user command error: {}", e);
            }
        }
        UnknownCommand { .. } => eprintln!("Somehow got an unknown command error?"),
        _ => eprintln!("UNHANDLED ERROR OCCURRED: {:?}", e),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
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
                Ok(Data {})
            })
        });
    framework.run().await.unwrap();
}
