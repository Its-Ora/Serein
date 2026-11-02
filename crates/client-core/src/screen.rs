//! Bounded screen-share settings and ephemeral Gateway negotiation events.
use crate::voice;
use model::Id;

pub const MAX_SOURCES: usize = 64;
pub const MAX_SOURCE_NAME_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceId {
	/// macOS chooses a display/window only after an explicit Share action.
	SystemPicker,
	/// The Linux desktop chooses the source after an explicit Share action.
	Portal,
	/// Explicit whole-desktop capture on a native X11 session, without a portal.
	X11Desktop,
	Display(u64),
	Window(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
	pub id: SourceId,
	pub name: String,
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
	pub source: SourceId,
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub cursor: bool,
	/// Share system audio with the screen; call microphone settings are independent.
	pub audio: bool,
}
/// Fork: owner-chosen stream bitrate in kbit/s; 0 keeps the automatic table.
static BITRATE_OVERRIDE_KBPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Highest manual stream bitrate accepted, in Mbit/s.
pub const MAX_MANUAL_MBPS: u32 = 80;

/// Set the manual stream bitrate (Mbit/s); 0 returns to automatic. Applies to the next
/// stream start and to the running stream's ceiling.
pub fn set_bitrate_override(mbps: u32) {
	BITRATE_OVERRIDE_KBPS.store(
		mbps.min(MAX_MANUAL_MBPS) * 1000,
		std::sync::atomic::Ordering::Relaxed,
	);
}
pub fn bitrate_override_mbps() -> u32 {
	BITRATE_OVERRIDE_KBPS.load(std::sync::atomic::Ordering::Relaxed) / 1000
}

impl Settings {
	pub fn valid(self) -> bool {
		!matches!(self.source, SourceId::Display(0) | SourceId::Window(0))
			&& matches!(
				(self.width, self.height),
				(854, 480) | (1280, 720) | (1920, 1080) | (2560, 1440)
			) && matches!(self.fps, 15 | 30 | 60)
	}

	/// Encoder ceiling. Fork: higher automatic rates (upstream's 1080p30 at 8 Mbit/s looked
	/// soft on films and games) and an optional manual value.
	pub fn bit_rate(self) -> u32 {
		let manual = BITRATE_OVERRIDE_KBPS.load(std::sync::atomic::Ordering::Relaxed);
		if manual > 0 {
			return manual.saturating_mul(1000).max(500_000);
		}
		let base: u32 = match (self.width, self.height) {
			(854, 480) => 3_000_000,
			(1920, 1080) => 10_000_000,
			(2560, 1440) => 16_000_000,
			_ => 6_000_000,
		};
		match self.fps {
			60 => base / 5 * 8,
			15 => base / 2,
			_ => base,
		}
		.min(40_000_000)
	}
}

#[derive(Debug)]
pub enum Event {
	Created {
		rtc_server: Id,
		rtc_channel: Id,
	},
	Server {
		token: Option<voice::Secret>,
		endpoint: Option<String>,
	},
	/// The stream is gone. `reason` names Discord's cause when it sent one we recognise.
	Deleted {
		reason: Option<&'static str>,
	},
	Failed(&'static str),
	/// Discord's latest viewer count for this device's own stream (unofficial `STREAM_UPDATE`).
	Viewers(u16),
}
impl Event {
	pub(crate) fn bytes(&self) -> usize {
		match self {
			Self::Server { token, endpoint } => {
				token.as_ref().map_or(0, voice::Secret::bytes)
					+ endpoint.as_ref().map_or(0, String::capacity)
			}
			_ => 0,
		}
	}
}
