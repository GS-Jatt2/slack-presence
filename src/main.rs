use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, TimeZone, Timelike, Weekday};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

const PRESENCE_API: &str = "https://slack.com/api/users.setPresence";
const PROFILE_API: &str = "https://slack.com/api/users.profile.set";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Auto,
    Away,
}

impl Presence {
    fn as_str(self) -> &'static str {
        match self {
            Presence::Auto => "auto",
            Presence::Away => "away",
        }
    }
}

#[derive(Deserialize)]
struct SlackResponse {
    ok: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct StatusProfile {
    status_text: String,
    status_emoji: String,
    status_expiration: i64,
}

#[derive(Debug, Clone)]
struct Config {
    token: String,
    active_hour: u32,
    away_hour: u32,
    oof_text: String,
    oof_emoji: String,
}

impl Config {
    fn from_env() -> Result<Self> {
        let token = env::var("SLACK_USER_TOKEN").context(
            "SLACK_USER_TOKEN env var is required (xoxp- user token with users:write and users.profile:write scopes)",
        )?;
        let active_hour = parse_hour("ACTIVE_HOUR", 10)?;
        let away_hour = parse_hour("AWAY_HOUR", 19)?;
        if active_hour == away_hour {
            return Err(anyhow!("ACTIVE_HOUR and AWAY_HOUR must differ"));
        }
        let oof_text = env::var("OOF_STATUS_TEXT").unwrap_or_else(|_| "Out of office".to_string());
        let oof_emoji = env::var("OOF_STATUS_EMOJI").unwrap_or_else(|_| ":palm_tree:".to_string());
        Ok(Self {
            token,
            active_hour,
            away_hour,
            oof_text,
            oof_emoji,
        })
    }

    fn presence_for(&self, now: &DateTime<Local>) -> Presence {
        if is_off_day(now) {
            return Presence::Away;
        }
        let h = now.hour();
        if self.active_hour < self.away_hour {
            if h >= self.active_hour && h < self.away_hour {
                Presence::Auto
            } else {
                Presence::Away
            }
        } else if h >= self.active_hour || h < self.away_hour {
            Presence::Auto
        } else {
            Presence::Away
        }
    }

    fn next_transition(&self, now: &DateTime<Local>) -> (DateTime<Local>, Presence) {
        (0..=7)
            .flat_map(|d| {
                let base = *now + ChronoDuration::days(d);
                [
                    (at_hour(&base, self.active_hour), Presence::Auto),
                    (at_hour(&base, self.away_hour), Presence::Away),
                ]
            })
            .filter(|(t, _)| t > now)
            .filter(|(t, p)| !(is_off_day(t) && *p == Presence::Auto))
            .min_by_key(|(t, _)| *t)
            .expect("a future transition always exists within a week")
    }

    fn next_active(&self, now: &DateTime<Local>) -> DateTime<Local> {
        (1..=7)
            .map(|d| at_hour(&(*now + ChronoDuration::days(d)), self.active_hour))
            .find(|t| t > now && !is_off_day(t))
            .expect("a future non-off active hour exists within a week")
    }
}

fn is_off_day(t: &DateTime<Local>) -> bool {
    t.weekday() == Weekday::Sun
}

fn parse_hour(var: &str, default: u32) -> Result<u32> {
    match env::var(var) {
        Ok(s) => {
            let n: u32 = s.parse().with_context(|| format!("{var} not an integer"))?;
            if n > 23 {
                return Err(anyhow!("{var} must be 0..=23"));
            }
            Ok(n)
        }
        Err(_) => Ok(default),
    }
}

fn at_hour(now: &DateTime<Local>, hour: u32) -> DateTime<Local> {
    Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), hour, 30, 0)
        .single()
        .expect("valid local time")
}

async fn set_presence(client: &Client, token: &str, presence: Presence) -> Result<()> {
    let resp = client
        .post(PRESENCE_API)
        .bearer_auth(token)
        .form(&[("presence", presence.as_str())])
        .send()
        .await
        .context("HTTP request to users.setPresence failed")?;

    let status = resp.status();
    let body: SlackResponse = resp
        .json()
        .await
        .context("could not decode users.setPresence response")?;

    if !body.ok {
        return Err(anyhow!(
            "Slack rejected setPresence (http {}): {}",
            status,
            body.error.unwrap_or_else(|| "unknown".into())
        ));
    }
    Ok(())
}

async fn set_profile_status(client: &Client, token: &str, profile: &StatusProfile) -> Result<()> {
    let resp = client
        .post(PROFILE_API)
        .bearer_auth(token)
        .json(&json!({ "profile": profile }))
        .send()
        .await
        .context("HTTP request to users.profile.set failed")?;

    let status = resp.status();
    let body: SlackResponse = resp
        .json()
        .await
        .context("could not decode users.profile.set response")?;

    if !body.ok {
        return Err(anyhow!(
            "Slack rejected profile.set (http {}): {}",
            status,
            body.error.unwrap_or_else(|| "unknown".into())
        ));
    }
    Ok(())
}

async fn apply(client: &Client, cfg: &Config, presence: Presence) -> Result<()> {
    set_presence(client, &cfg.token, presence).await?;
    let profile = match presence {
        Presence::Away => StatusProfile {
            status_text: cfg.oof_text.clone(),
            status_emoji: cfg.oof_emoji.clone(),
            status_expiration: cfg.next_active(&Local::now()).timestamp(),
        },
        Presence::Auto => StatusProfile {
            status_text: String::new(),
            status_emoji: String::new(),
            status_expiration: 0,
        },
    };
    set_profile_status(client, &cfg.token, &profile).await?;
    Ok(())
}

async fn apply_with_retry(client: &Client, cfg: &Config, presence: Presence) {
    let mut backoff = Duration::from_secs(5);
    for attempt in 1..=6 {
        match apply(client, cfg, presence).await {
            Ok(()) => {
                info!(presence = presence.as_str(), "presence + status updated");
                return;
            }
            Err(e) => {
                warn!(attempt, error = %e, "apply failed, will retry");
                sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(300));
            }
        }
    }
    error!("giving up after retries; will catch up at next transition");
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env()?;
    info!(
        active_hour = cfg.active_hour,
        away_hour = cfg.away_hour,
        oof_text = %cfg.oof_text,
        oof_emoji = %cfg.oof_emoji,
        "starting slack-presence daemon"
    );

    let client = Client::builder().timeout(Duration::from_secs(15)).build()?;

    let now = Local::now();
    let desired = cfg.presence_for(&now);
    info!(
        presence = desired.as_str(),
        "applying current-state on startup"
    );
    apply_with_retry(&client, &cfg, desired).await;

    loop {
        let now = Local::now();
        let (at, presence) = cfg.next_transition(&now);
        let wait = (at - now).to_std().unwrap_or(Duration::from_secs(1));
        info!(
            target = %at.format("%Y-%m-%d %H:%M:%S %Z"),
            in_seconds = wait.as_secs(),
            next = presence.as_str(),
            "sleeping until next transition"
        );

        tokio::select! {
            _ = sleep(wait) => {
                apply_with_retry(&client, &cfg, presence).await;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown requested, exiting");
                return Ok(());
            }
        }
    }
}
