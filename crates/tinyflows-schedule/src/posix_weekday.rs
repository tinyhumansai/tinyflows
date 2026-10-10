// SPDX-License-Identifier: GPL-3.0-or-later
//! POSIX crontab weekday numbers, rewritten into the `cron` crate's.
//!
//! A standard 5-field crontab numbers the days of the week 0–7, with both 0
//! and 7 meaning Sunday. The `cron` crate numbers them 1–7 with 1 = Sunday,
//! so the same digit names the next day over: `5` is Friday to crontab and
//! Thursday to the crate. Day names (`MON`, `fri`) mean the same to both and
//! are left alone.

use anyhow::{Result, bail};

/// The highest POSIX weekday number; like 0, it means Sunday.
const POSIX_SUNDAY_ALIAS: u32 = 7;

/// The `cron` crate's number for Sunday.
const CRATE_SUNDAY: u32 = 1;

/// Rewrites a POSIX day-of-week field (`5`, `1-5`, `*/2`, `5-7`, `MON,3`) into
/// the `cron` crate's numbering, firing on the same days.
pub(crate) fn posix_to_crate_weekdays(field: &str) -> Result<String> {
    let items = field
        .split(',')
        .map(translate_item)
        .collect::<Result<Vec<_>>>()?;
    Ok(items.join(","))
}

/// One comma-separated item: a day, a range, either with a `/step`, or a
/// wildcard or name.
fn translate_item(item: &str) -> Result<String> {
    let (base, step) = match item.split_once('/') {
        Some((base, step)) => (base, Some(parse_step(step)?)),
        None => (item, None),
    };
    // A wildcard with a step needs to be rewritten. POSIX `*/n` walks 0, n, 2n…
    // over days 0–7, but the crate walks 1, 1+n, 1+2n… over days 1–7, so we
    // must emit an explicit range `1-7/n` to get the right days.
    // A bare wildcard or a name needs no translation.
    if (base == "*" || base == "?") && step.is_some() {
        // Stepped wildcard: rewrite to explicit crate range.
        let step_val = step.unwrap();
        return Ok(format!("1-7/{step_val}"));
    }
    if base == "*" || base == "?" || base.chars().any(|c| c.is_ascii_alphabetic()) {
        return Ok(item.to_string());
    }
    let (start, end) = match base.split_once('-') {
        Some((start, end)) => (posix_day(start)?, posix_day(end)?),
        // `n/step` runs from `n` to the end of the week; a bare `n` is one day.
        None if step.is_some() => (posix_day(base)?, POSIX_SUNDAY_ALIAS),
        None => return Ok(crate_day(posix_day(base)?).to_string()),
    };
    if start > end {
        bail!("day of week range `{item}` runs backwards");
    }
    let step = step.unwrap_or(1);
    let suffix = if step == 1 {
        String::new()
    } else {
        format!("/{step}")
    };
    if end < POSIX_SUNDAY_ALIAS {
        return Ok(format!("{}-{}{suffix}", start + 1, end + 1));
    }
    // The range runs into 7, which is Sunday again: the crate's 1, at the
    // other end of its week. Days start..=6 shift up one as usual; whether the
    // walk also lands on 7 decides whether Sunday is appended.
    if start == 0 {
        // Starting on Sunday already covers it.
        return Ok(format!("{CRATE_SUNDAY}-7{suffix}"));
    }
    let lands_on_sunday = (POSIX_SUNDAY_ALIAS - start) % step == 0;
    let mut parts = Vec::with_capacity(2);
    if start < POSIX_SUNDAY_ALIAS {
        parts.push(format!("{}-7{suffix}", start + 1));
    }
    if lands_on_sunday {
        parts.push(CRATE_SUNDAY.to_string());
    }
    Ok(parts.join(","))
}

fn posix_day(text: &str) -> Result<u32> {
    match text.parse::<u32>() {
        Ok(day) if day <= POSIX_SUNDAY_ALIAS => Ok(day),
        _ => bail!("day of week `{text}` must be a number from 0 to 7 or a day name"),
    }
}

fn parse_step(text: &str) -> Result<u32> {
    match text.parse::<u32>() {
        Ok(step) if step > 0 => Ok(step),
        _ => bail!("day of week step `{text}` must be a positive number"),
    }
}

/// POSIX day → crate day: 0 and 7 are Sunday (1), the rest shift up one.
fn crate_day(posix: u32) -> u32 {
    if posix == 0 || posix == POSIX_SUNDAY_ALIAS {
        CRATE_SUNDAY
    } else {
        posix + 1
    }
}
