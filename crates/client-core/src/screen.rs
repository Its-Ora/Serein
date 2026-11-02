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
/// Fork: the Go Live bitrate Discord is reported to accept (about 10 Mbit/s; above it the
/// stream loses packets for every viewer). Every stream aims for it; Discord's own REMB
/// estimate still lowers it on a congested path.
pub const DISCORD_STREAM_CAP: u32 = 10_000_000;
/// Film cadence: `fps == 23` means 24000/1001 (23.976) frames per second.
pub const FILM_FPS: u32 = 23;

impl Settings {
	pub fn valid(self) -> bool {
		!matches!(self.source, SourceId::Display(0) | SourceId::Window(0))
			&& matches!(
				(self.width, self.height),
				(854, 480) | (1280, 720) | (1664, 936) | (1920, 1080)
			) && matches!(self.fps, 15 | FILM_FPS | 30 | 60 | 120)
	}

	/// Encoder ceiling. Fork: always the most Discord accepts, whatever the resolution
	/// (upstream used 2-16 Mbit/s by resolution, so most streams sent far less).
	pub fn bit_rate(self) -> u32 {
		DISCORD_STREAM_CAP
	}

	/// Whole frames per second for encoders and signaling (23.976 rounds to 24).
	pub fn nominal_fps(self) -> u32 {
		if self.fps == FILM_FPS { 24 } else { self.fps }
	}

	/// Exact capture/encode pacing, including the 24000/1001 film cadence.
	pub fn frame_interval(self) -> std::time::Duration {
		if self.fps == FILM_FPS {
			std::time::Duration::from_nanos(1_001_000_000 / 24)
		} else {
			std::time::Duration::from_secs_f64(1.0 / f64::from(self.fps.max(1)))
		}
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
