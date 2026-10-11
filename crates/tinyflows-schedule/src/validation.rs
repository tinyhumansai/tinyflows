//! Portable human-delay and delivery validation; hosts supply channel access.
use crate::{DeliveryConfig, JobOrigin, SessionTarget, delivery_mode};
use anyhow::Result;

/// Parse s/m/h/d suffixes, defaulting bare numbers to minutes.
/// Checked arithmetic rejects a duration too large to represent.
pub fn parse_human_delay(input: &str) -> Result<chrono::Duration> {
    let input = input.trim();
    anyhow::ensure!(!input.is_empty(), "delay must not be empty");
    let split = input
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(input.len());
    let (num, unit) = input.split_at(split);
    let amount: i64 = num.parse()?;
    let multiplier = match unit {
        "s" => 1,
        "" | "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => anyhow::bail!("unsupported delay unit '{unit}', use s/m/h/d"),
    };
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("delay is too large"))?;
    chrono::Duration::try_seconds(seconds).ok_or_else(|| anyhow::anyhow!("delay is too large"))
}

/// Reject origin delivery or a current-session target without a conversation snapshot.
pub fn check_origin_requirements(
    target: &SessionTarget,
    delivery: &DeliveryConfig,
    origin: Option<&JobOrigin>,
) -> Result<(), String> {
    if origin.is_some() {
        return Ok(());
    }
    if delivery
        .mode
        .trim()
        .eq_ignore_ascii_case(delivery_mode::ORIGIN)
    {
        return Err("delivery mode 'origin' needs the conversation the job is created from; this job has none (use 'proactive' or 'announce')".into());
    }
    if matches!(target, SessionTarget::Current) {
        return Err("session_target 'current' needs the conversation the job is created from; this job has none (use 'isolated')".into());
    }
    Ok(())
}

/// None is an unknown/unconfigured channel; Some([]) permits every recipient.
pub fn validate_delivery(
    delivery: &DeliveryConfig,
    origin: Option<&JobOrigin>,
    allowed_users: Option<&[String]>,
) -> Result<(), String> {
    let mode = delivery.mode.trim();
    if mode.eq_ignore_ascii_case(delivery_mode::ORIGIN) {
        return check_origin_requirements(&SessionTarget::Isolated, delivery, origin);
    }
    if !mode.eq_ignore_ascii_case(delivery_mode::ANNOUNCE) {
        return Ok(());
    }
    let channel = delivery
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("delivery.channel is required for announce mode")?;
    let to = delivery
        .to
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("delivery.to is required for announce mode")?;
    if let Some(JobOrigin::Channel {
        channel: original,
        reply_target,
        ..
    }) = origin
    {
        if channel.eq_ignore_ascii_case(original) && to == reply_target {
            return Ok(());
        }
    }
    if channel.eq_ignore_ascii_case("web") {
        return Ok(());
    }
    match allowed_users {
        Some(users) if users.is_empty() || users.iter().any(|u| u == to) => Ok(()),
        Some(_) => Err(format!(
            "delivery target '{to}' on channel '{channel}' is not in allowed_users for that channel; refusing to schedule cross-tenant delivery"
        )),
        None => Err(format!(
            "delivery channel '{channel}' is not configured; cannot validate target"
        )),
    }
}

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
