//! Small local log of why sessions end or requests are refused (HTTP 401, Gateway closes).
//!
//! Only fixed labels, request paths with every number replaced by `{id}`, and UTC times are
//! written. Never tokens, headers, bodies, message contents or account identifiers. The file
//! is bounded: once it passes `MAX_BYTES` it is cut to its newer half.
use std::{
	io::Write,
	path::PathBuf,
	sync::{Mutex, OnceLock},
};

const MAX_BYTES: u64 = 64 * 1024;
static PATH: OnceLock<PathBuf> = OnceLock::new();
static WRITE: Mutex<()> = Mutex::new(());

/// Set once by the desktop app, inside its private data directory.
pub fn set_path(path: PathBuf) {
	let _ = PATH.set(path);
}

/// Where the log lives, for showing to the owner.
pub fn path() -> Option<&'static PathBuf> {
	PATH.get()
}

/// Strips the query and replaces every run of digits, so no ID ever reaches the file.
pub fn redact_path(path: &str) -> String {
	let path = path.split('?').next().unwrap_or_default();
	let mut out = String::with_capacity(path.len().min(256));
	let mut digits = false;
	for character in path.chars().take(256) {
		if character.is_ascii_digit() {
			if !digits {
				out.push_str("{id}");
			}
			digits = true;
		} else {
			digits = false;
			// Keep only path-safe characters; anything else becomes `_`.
			out.push(
				if character.is_ascii_alphanumeric()
					|| matches!(character, '/' | '-' | '_' | '@' | '.')
				{
					character
				} else {
					'_'
				},
			);
		}
	}
	out
}

/// Appends one line. Failures are ignored: logging must never break a session.
pub fn record(line: &str) {
	let Some(path) = PATH.get() else {
		return;
	};
	let Ok(_guard) = WRITE.lock() else {
		return;
	};
	if std::fs::metadata(path).is_ok_and(|meta| meta.len() > MAX_BYTES)
		&& let Ok(content) = std::fs::read_to_string(path)
	{
		let mut keep = content.len() / 2;
		while !content.is_char_boundary(keep) {
			keep += 1;
		}
		let start = content[keep..]
			.find('\n')
			.map_or(content.len(), |at| keep + at + 1);
		let _ = std::fs::write(path, &content[start..]);
	}
	let Ok(mut file) = std::fs::OpenOptions::new()
		.create(true)
		.append(true)
		.open(path)
	else {
		return;
	};
	let line: String = line.chars().filter(|c| !c.is_control()).take(400).collect();
	let _ = writeln!(file, "{} UTC  {line}", utc_now());
}

fn utc_now() -> String {
	let secs = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |elapsed| elapsed.as_secs()) as i64;
	let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
	// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z - era * 146_097;
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let day = doy - (153 * mp + 2) / 5 + 1;
	let month = if mp < 10 { mp + 3 } else { mp - 9 };
	let year = yoe + era * 400 + i64::from(month <= 2);
	format!(
		"{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
		rest / 3600,
		rest % 3600 / 60,
		rest % 60
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn redacted_paths_carry_no_ids_or_queries() {
		assert_eq!(
			redact_path("/channels/1234567890/messages/987?limit=50&token=x"),
			"/channels/{id}/messages/{id}"
		);
		assert_eq!(
			redact_path("/users/@me/mentions?limit=50"),
			"/users/@me/mentions"
		);
		assert_eq!(redact_path("/a b\"c"), "/a_b_c");
	}
}
