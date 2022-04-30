use chrono::{prelude::*, Duration as ChronoDuration};
use eyre::{Result, WrapErr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use structopt::StructOpt;
use termcolor::{ColorChoice, StandardStream};
use tokio::time::sleep;

#[cfg(feature = "rusoto")]
use rusoto_cloudformation::CloudFormationClient;
#[cfg(feature = "rusoto")]
use rusoto_core::Region;

#[cfg(feature = "aws-sdk")]
use aws_sdk_cloudformation::Client;

mod aws;
mod error;
mod nested_stacks;
mod stack_status;
mod stacks;
mod tail;
mod utils;
mod writer;

use crate::error::Error;
use crate::stacks::build_stack_list;
use crate::tail::{Tail, TailConfig};
use crate::writer::Writer;

// Custom parser for parsing the datetime as either a timestamp, or as a handy string.
fn parse_since_argument(src: &str) -> Result<DateTime<Utc>> {
    // Try to parse as datetime
    if let Ok(dt) = DateTime::from_str(src) {
        return Ok(dt);
    }

    // Try to parse as naive datetime (and assume UTC)
    if let Ok(dt) = NaiveDateTime::from_str(src).map(|n| DateTime::<Utc>::from_utc(n, Utc)) {
        return Ok(dt);
    }

    // Try to parse as timestamp
    if let Ok(dt) = src.parse::<i64>().map(|i| Utc.timestamp(i, 0)) {
        return Ok(dt);
    }

    // some common terms
    if src == "today" {
        let today = Utc::today();
        let dt = today.and_hms(0, 0, 0);
        return Ok(dt);
    } else if src == "yesterday" {
        let yesterday = Utc::today() - ChronoDuration::days(1);
        let dt = yesterday.and_hms(0, 0, 0);
        return Ok(dt);
    }

    Err(Error::ParseSince).wrap_err("error parsing since argument")
}

#[derive(StructOpt)]
#[structopt(author = "Simon Walker")]
/// Tail CloudFormation deployments
///
/// Watch a log of deployment events for CloudFormation stacks from your console.
struct Opts {
    /// Name of the stacks to tail
    stack_names: Vec<String>,

    /// When to start fetching data from. This could be a timestamp, text string, or the words
    /// `today` or `yesterday`
    #[structopt(short, long, parse(try_from_str = parse_since_argument))]
    since: Option<DateTime<Utc>>,

    /// Also fetch nested stacks and their deploy status
    #[structopt(short, long)]
    nested: bool,

    /// Do not print stack separators
    #[structopt(long)]
    no_show_separators: bool,

    // Do not show notifications
    #[structopt(long)]
    no_show_notifications: bool,

    // Do not print stack outputs on completion
    #[structopt(long)]
    no_show_outputs: bool,
}

#[cfg(feature = "rusoto")]
async fn create_client() -> CloudFormationClient {
    let region = Region::default();
    tracing::debug!(region = ?region, "chosen region");

    CloudFormationClient::new(region)
}

#[cfg(feature = "aws-sdk")]
async fn create_client() -> aws_sdk_cloudformation::Client {
    let config = aws_config::load_from_env().await;
    Client::new(&config)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    color_eyre::install().unwrap();

    let opts = Opts::from_args();
    let since = opts.since.unwrap_or_else(Utc::now);

    tracing::info!(stack_names = ?opts.stack_names, since = %since, nested = ?opts.nested, "tailing stack events");

    if opts.stack_names.is_empty() {
        let mut app = Opts::clap();
        eprintln!("Error: no stacks specified");
        app.print_help().unwrap();
        std::process::exit(1);
    }

    let mut stdout = StandardStream::stdout(ColorChoice::Auto);
    let mut writer = Writer::new(&mut stdout);

    loop {
        let client = create_client().await;
        let stack_info = build_stack_list(&client, &opts.stack_names, opts.nested)
            .await
            .expect("building stack list");

        let config = TailConfig {
            since,
            stack_info: &stack_info,
            show_separators: !opts.no_show_separators,
            show_notifications: !opts.no_show_notifications,
            show_outputs: !opts.no_show_outputs,
        };

        let mut tail = Tail::new(config, Arc::new(client), &mut writer);

        tracing::info!("prefetching tasks");
        match tail.prefetch().await {
            Ok(_) => {}
            Err(e) => match e.downcast_ref::<Error>() {
                Some(Error::NoCredentials) => {
                    eprintln!("Error: no valid credentials found");
                    std::process::exit(1);
                }
                Some(Error::NoStack(stack_name)) => {
                    eprintln!("Error: could not find stack {}", stack_name);
                    std::process::exit(1);
                }
                Some(Error::CredentialsExpired) => {
                    eprintln!("Error: your credentials have expired");
                    std::process::exit(1);
                }
                Some(Error::RateLimitExceeded) => {
                    tracing::warn!("rate limit exceeded");
                    sleep(Duration::from_secs(5)).await;
                }
                Some(e) => {
                    eprintln!("Error: unknown error: {:?}", e);
                    std::process::exit(1);
                }
                None => {
                    eprintln!("Error: unknown error: {:?}", e);
                    std::process::exit(1);
                }
            },
        }

        tracing::debug!("starting poll loop");
        match tail.poll().await {
            Ok(_) => unreachable!(),
            Err(e) => match e.downcast_ref::<Error>() {
                Some(Error::CredentialsExpired) => {
                    eprintln!("Error: your credentials have expired");
                    std::process::exit(1);
                }
                Some(Error::RateLimitExceeded) => {
                    tracing::warn!("rate limit exceeded");
                    sleep(Duration::from_secs(5)).await;
                }
                Some(Error::NoStack(name)) => {
                    eprintln!("could not find stack {}", name);
                    std::process::exit(1);
                }
                Some(e) => {
                    tracing::error!(err = %e, "unexpected error");
                    std::process::exit(1);
                }
                None => {
                    tracing::error!(err = %e, "unexpected error");
                    std::process::exit(1);
                }
            },
        }

        tracing::trace!("building another client");
    }
}
