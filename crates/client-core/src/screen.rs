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
	/// Native pixel size. Zero means the platform has not reported one yet.
	pub width: u32,
	pub height: u32,
	/// Monitor refresh in hertz. Zero means unknown, which hides 90 and 120 fps.
	pub refresh_hz: u32,
}

/// A share picture the picker can offer. `Source` is the monitor or window's own size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
	Source,
	Preset { width: u32, height: u32 },
}

pub const MAX_SHARE_WIDTH: u32 = 3840;
pub const MAX_SHARE_HEIGHT: u32 = 2160;

// Fork: the owner's list, with 936p as the best fit for Discord's ~10 Mbit/s.
const PRESETS: [Resolution; 4] = [
	Resolution::Preset {
		width: 1920,
		height: 1080,
	},
	Resolution::Preset {
		width: 1664,
		height: 936,
	},
	Resolution::Preset {
		width: 1280,
		height: 720,
	},
	Resolution::Preset {
		width: 854,
		height: 480,
	},
];

/// Resolutions that fit inside a source. Unknown size keeps 1080p, 720p, and 480p.
pub fn offered_resolutions(width: u32, height: u32) -> impl Iterator<Item = Resolution> {
	let known = width > 0 && height > 0;
	// Fork: no "source size" entry; the owner picks from the fixed list.
	PRESETS.into_iter().filter(move |preset| {
		let Resolution::Preset {
			width: preset_width,
			height: preset_height,
		} = *preset
		else {
			return false;
		};
		if known {
			width >= preset_width && height >= preset_height
		} else {
			preset_height <= 1080
		}
	})
}

/// 15, 23.976 (film), 30, and 60 always. 90 and 120 only when the monitor refreshes at
/// least that fast.
pub fn offered_frame_rates(refresh_hz: u32) -> impl Iterator<Item = u32> {
	[15, FILM_FPS, 30, 60, 90, 120]
		.into_iter()
		.filter(move |fps| *fps <= 60 || refresh_hz >= *fps)
}

/// Output size for a choice. `None` when that choice does not fit the source.
pub fn picture(resolution: Resolution, width: u32, height: u32) -> Option<(u32, u32)> {
	match resolution {
		Resolution::Source => fitted_source(width, height),
		Resolution::Preset {
			width: preset_width,
			height: preset_height,
		} => {
			let known = width > 0 && height > 0;
			if known && (width < preset_width || height < preset_height) {
				return None;
			}
			if !known && preset_height > 1080 {
				return None;
			}
			Some((preset_width, preset_height))
		}
	}
}

fn fitted_source(width: u32, height: u32) -> Option<(u32, u32)> {
	if width < 2 || height < 2 {
		return None;
	}
	if width <= MAX_SHARE_WIDTH && height <= MAX_SHARE_HEIGHT {
		let width = width & !1;
		let height = height & !1;
		return (width >= 2 && height >= 2).then_some((width, height));
	}
	let width = u64::from(width);
	let height = u64::from(height);
	let scale = ((u64::from(MAX_SHARE_WIDTH) << 16) / width)
		.min((u64::from(MAX_SHARE_HEIGHT) << 16) / height);
	let fitted_width = u32::try_from((width * scale) >> 16).unwrap_or(0) & !1;
	let fitted_height = u32::try_from((height * scale) >> 16).unwrap_or(0) & !1;
	(fitted_width >= 2 && fitted_height >= 2).then_some((fitted_width, fitted_height))
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
			&& self.width >= 2
			&& self.height >= 2
			&& self.width <= MAX_SHARE_WIDTH
			&& self.height <= MAX_SHARE_HEIGHT
			&& self.width.is_multiple_of(2)
			&& self.height.is_multiple_of(2)
			&& matches!(self.fps, 15 | FILM_FPS | 30 | 60 | 90 | 120)
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
